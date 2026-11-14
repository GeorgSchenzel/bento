//! Boxes: one sandbox per forest (a single checkout is a forest of one).
//! A box is the pinned nixos/nix image with the shared store mounted
//! read-only; nix goes through the builder's daemon (socat forwarder on
//! /tmp/nix.sock), and the box tools are on PATH.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context as _, Result};
use microsandbox::sandbox::{HostPermissions, Sandbox, SandboxHandle, StatVirtualization};
use microsandbox::{MicrosandboxError, NetworkPolicy, NetworkProfile};
use microsandbox_image::ext4::{format_ext4, Ext4FormatOptions};

use crate::agent;
use crate::bridge;
use crate::builder;
use crate::forest::{self, Forest, WORKDIR};
use crate::herdr;
use crate::store;
use crate::util;

/// Linked-worktree git dirs inside the box: each tree's main git dir is
/// bound rw at /git/<tree> (commits write objects/refs/index through it).
const GUEST_GIT: &str = "/git";
const NIX_SOCK: &str = "/tmp/nix.sock";
const DOCKER_ROOT: &str = "/var/lib/docker";
const DOCKER_SOCK: &str = "/var/run/docker.sock";

/// One mount: a bind, or an ext4 disk image. Writable binds always carry a
/// quota: msb's default is 4 GiB of guest-added bytes, past which writes
/// fail with ENOSPC.
#[derive(Debug)]
pub struct Mount {
    guest: String,
    host: PathBuf,
    kind: MountKind,
}

#[derive(Debug)]
enum MountKind {
    Ro,
    Rw(u32),
    /// Rw for the host's live files, see [`Mount::live`].
    Live(u32),
    Disk,
}

impl Mount {
    pub fn ro(guest: impl AsRef<Path>, host: impl Into<PathBuf>) -> Self {
        Mount {
            guest: guest.as_ref().display().to_string(),
            host: host.into(),
            kind: MountKind::Ro,
        }
    }

    pub fn rw(guest: impl AsRef<Path>, host: impl Into<PathBuf>, quota_mib: u32) -> Self {
        Mount {
            guest: guest.as_ref().display().to_string(),
            host: host.into(),
            kind: MountKind::Rw(quota_mib),
        }
    }

    /// A writable bind of files the host works with too: checkouts and their
    /// git dirs. On Linux msb's default keeps guest symlinks and modes in
    /// xattrs, so the host would see symlinks as regular files, no exec bits,
    /// and a dirty `git status`.
    fn live(guest: impl AsRef<Path>, host: impl Into<PathBuf>, quota_mib: u32) -> Self {
        Mount {
            guest: guest.as_ref().display().to_string(),
            host: host.into(),
            kind: MountKind::Live(quota_mib),
        }
    }

    fn disk(guest: impl AsRef<Path>, host: impl Into<PathBuf>) -> Self {
        Mount {
            guest: guest.as_ref().display().to_string(),
            host: host.into(),
            kind: MountKind::Disk,
        }
    }
}

/// Host-side per-box dir: the /workspace root, overlays, generated git
/// files and agent state.
pub fn state_dir(forest: &Forest) -> Result<PathBuf> {
    box_state_dir(&forest.name)
}

/// A box's state dir by box name. Pub for herdr's unregister, which looks a
/// box up by name alone when the forest itself is already gone.
pub fn box_state_dir(name: &str) -> Result<PathBuf> {
    Ok(util::data_dir()?.join("boxes").join(name))
}

fn env(home: &Path, forest: &Forest) -> Result<Vec<(String, String)>> {
    let mut env = vec![
        ("HOME".into(), home.display().to_string()),
        ("NIX_REMOTE".into(), format!("unix://{NIX_SOCK}")),
        (
            "NIX_CONFIG".into(),
            "experimental-features = nix-command flakes".into(),
        ),
        (
            "TERMINFO_DIRS".into(),
            format!("{}/share/terminfo", builder::TOOLS),
        ),
        (
            "PATH".into(),
            format!(
                "{}/bin:/nix/var/nix/profiles/default/bin:/usr/bin:/bin",
                builder::TOOLS
            ),
        ),
    ];
    env.extend(agent::sandbox_env(home));
    // The bridge: how this box reaches the host daemon (`bento serve`).
    // The identity file is created here (read-or-create), so the token is
    // fixed the first time a box is used and survives recreation.
    let info = bridge::ensure_box_info(&state_dir(forest)?, &forest.name, &forest.dir)?;
    env.push((
        "BENTO_BRIDGE".into(),
        format!("{}:{}", bridge::HOST, info.port),
    ));
    env.push(("BENTO_BRIDGE_TOKEN".into(), info.token));
    env.push(("BENTO_BOX".into(), forest.name.clone()));
    if let Some(name) = bridge::workforest_name(&forest.dir) {
        env.push(("BENTO_FOREST".into(), name));
    }
    Ok(env)
}

fn mounts(forest: &Forest, nix: &Path, home: &Path, state: &Path) -> Result<Vec<Mount>> {
    // /workspace is box-owned (the trees are bound into it), so anything
    // written at the forest root stays off the host forest
    let root = state.join("root");
    fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    let mut mounts = vec![
        Mount::ro("/nix", nix),
        Mount::rw(WORKDIR, &root, forest.quota_mib()),
    ];
    remove_combined_devenv(&root)?;
    write_guide(forest, &root)?;
    for tree in &forest.trees {
        let quota = tree.quota_mib;
        let guest = format!("{WORKDIR}/{}", tree.name);
        mounts.push(Mount::live(&guest, &tree.dir, quota));
        // Box-local copies of platform-specific dirs: the tree bind is
        // shadowed per entry by a twin in the state dir, so host and box
        // build artifacts never mix.
        for entry in &tree.overlays {
            let host = state.join("ov").join(&tree.name).join(entry);
            fs::create_dir_all(&host).with_context(|| format!("creating {}", host.display()))?;
            mounts.push(Mount::rw(format!("{guest}/{entry}"), host, quota));
        }
        if let Some(wt) = &tree.wt {
            // The worktree's .git file and the worktrees/<id>/gitdir
            // back-pointer hold host paths; inside the box they must
            // reference /workspace and /git, so both are shadowed by
            // generated files.
            let git = format!("{GUEST_GIT}/{}", tree.name);
            let pointer = write(
                state,
                &format!("gitdir-pointer-{}", tree.name),
                &format!("gitdir: {git}/worktrees/{}\n", wt.wt_id),
            )?;
            let back = write(
                state,
                &format!("gitdir-back-{}", tree.name),
                &format!("{guest}/.git\n"),
            )?;
            mounts.push(Mount::live(&git, &wt.main_git, quota));
            mounts.push(Mount::ro(format!("{guest}/.git"), pointer));
            mounts.push(Mount::ro(
                format!("{git}/worktrees/{}/gitdir", wt.wt_id),
                back,
            ));
            // The main git dir is writable so commits write objects and
            // refs on the host — but never the parts host-side git would
            // execute from (config: core.fsmonitor/hooksPath; hooks/;
            // info/attributes), or the worktree's config.worktree.
            mounts.extend(git_shadows(&git, &wt.main_git, Some(&wt.wt_id)));
        } else if tree.dir.join(".git").is_dir() {
            // A main checkout's git dir sits inside its tree bind; the same
            // host-executable parts stay read-only there too.
            mounts.extend(git_shadows(
                &format!("{guest}/.git"),
                &tree.dir.join(".git"),
                None,
            ));
        }
    }
    mounts.push(Mount::ro(
        home.join(".gitconfig"),
        write_gitconfig(forest, state)?,
    ));
    // The single file only, not the dir: the box's own ~/.config/herdr-core
    // (the box core's state) must stay beside it.
    mounts.push(Mount::ro(
        home.join(".config/herdr-core/templates/opencode.md"),
        write(state, "hc-template-opencode.md", HC_TEMPLATE)?,
    ));
    mounts.push(Mount::disk(
        DOCKER_ROOT,
        docker_disk(state, forest.quota_mib())?,
    ));
    mounts.extend(agent::mounts(home, state, forest.quota_mib())?);
    Ok(mounts)
}

/// Read-only shadows over a writable git-dir bind: the parts host-side git
/// would execute from — `config` (a `core.fsmonitor` or `core.hooksPath`
/// planted in a box), `hooks/`, `info/` (attributes run diff drivers) and,
/// for a linked worktree, its `config.worktree` — stay host-controlled. The
/// same mechanism as the generated pointer files: a later, more specific
/// bind shadows the writable one. Missing host paths are skipped (nothing
/// to shadow); everything else in the git dir stays writable, so commits
/// (objects, refs, the worktree's HEAD and index) still reach the host.
fn git_shadows(guest_git: &str, host_git: &Path, wt: Option<&str>) -> Vec<Mount> {
    let mut mounts = Vec::new();
    for name in ["config", "hooks", "info"] {
        let host = host_git.join(name);
        if host.exists() {
            mounts.push(Mount::ro(format!("{guest_git}/{name}"), host));
        }
    }
    if let Some(id) = wt {
        let host = host_git.join("worktrees").join(id).join("config.worktree");
        if host.exists() {
            mounts.push(Mount::ro(
                format!("{guest_git}/worktrees/{id}/config.worktree"),
                host,
            ));
        }
    }
    mounts
}

/// Older boxes generated a combined devenv at the forest root; drop it so
/// the root stays shell-less.
fn remove_combined_devenv(root: &Path) -> Result<()> {
    for name in ["devenv.nix", "devenv.yaml"] {
        let path = root.join(name);
        match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => {}
        }
    }
    Ok(())
}

/// The agents' guide to the box, at /workspace (CLAUDE.md and AGENTS.md,
/// picked up from parent dirs). Rewritten on every connect, so the
/// checkouts' branches stay current.
fn write_guide(forest: &Forest, root: &Path) -> Result<()> {
    let mut checkouts = String::new();
    for tree in &forest.trees {
        let branch = forest::git(&tree.dir, &["branch", "--show-current"])
            .unwrap_or_else(|| "detached".into());
        checkouts.push_str(&format!("- `{WORKDIR}/{}` ({branch})\n", tree.name));
    }
    let other = if forest.trees.len() > 1 {
        " Your\n  shell has the one you started in; for another checkout run\n  \
         `cd /workspace/<project> && devenv shell -- <cmd>`."
    } else {
        ""
    };
    let arch = std::env::consts::ARCH;
    let host = if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "Linux"
    };
    let text = format!(
        "# Workspace

You are in a sandbox: a Linux VM ({arch}) on the user's {host} machine,
running as root with full network access. Experiment freely.

## Checkouts

{checkouts}
These are the user's live files: edits show up on their machine right away,
and `git commit` lands in their repo. `git push` doesn't work; leave that to
the user. Files outside the checkouts never leave the box.

Each repo's git config, hooks and info dirs are read-only here (host-side
git would run code planted in them); commits, branches and worktree reads
work as usual, `git config`/new hooks in the repo do not.

## Tools

- Each checkout with a `devenv.nix` has its tools in its devenv shell.{other}
  A first activation can take minutes.
- If a checkout has a `justfile`, run `just` to see its recipes and prefer
  them.
- Anything else: `nix shell nixpkgs#<pkg> -c <cmd>`.
- Docker works.
"
    );
    for name in ["CLAUDE.md", "AGENTS.md"] {
        write(root, name, &text)?;
    }
    Ok(())
}

/// Docker's data root: an ext4 image in the state dir (docker's overlayfs
/// can't stack on the box's overlay rootfs). It outlives box recreation, so
/// pulled images and volumes survive `bento start`; `bento rm` deletes it.
/// Sparse: the host only pays for what is written.
fn docker_disk(state: &Path, size_mib: u32) -> Result<PathBuf> {
    let path = state.join("docker.raw");
    if !path.exists() {
        let opts = Ext4FormatOptions {
            size_bytes: u64::from(size_mib) * 1024 * 1024,
            ..Default::default()
        };
        format_ext4(&path, &opts).map_err(|e| anyhow!("formatting {}: {e:?}", path.display()))?;
    }
    Ok(path)
}

fn write(state: &Path, name: &str, text: &str) -> Result<PathBuf> {
    let path = state.join(name);
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Minimal in-box git config: identity from the host, no signing (host keys
/// and helpers are unavailable in the box), and every dir safe (the binds
/// are owned by the host user, the box runs as root).
fn write_gitconfig(forest: &Forest, state: &Path) -> Result<PathBuf> {
    let dir = &forest.trees[0].dir;
    let (Some(name), Some(email)) = (
        forest::git(dir, &["config", "--get", "user.name"]),
        forest::git(dir, &["config", "--get", "user.email"]),
    ) else {
        bail!("git identity is not configured; set it on the host with:\n  git config --global user.name <name>\n  git config --global user.email <email>");
    };
    let text = format!(
        "[user]\n\tname = {name}\n\temail = {email}\n\
         [commit]\n\tgpgsign = false\n\
         [tag]\n\tgpgsign = false\n\
         [safe]\n\tdirectory = *\n"
    );
    write(state, "gitconfig", &text)
}

/// The hc template every box gets (decisions §4): opencode as an agent,
/// auto-approving since the box is the sandbox; --standalone gives each agent
/// its own server (a shared background service collides on its port), and
/// opencode reports its status (OSC 7501) itself, so hc waits for that. Mounted as a single file,
/// so anything else the box keeps under ~/.config/herdr-core stays visible.
const HC_TEMPLATE: &str = "---\ndescription: opencode agent (auto-approves; the box is the sandbox)\ncommand: opencode --standalone --auto\n---\n";

/// Names are path-derived; refuse to touch a box another checkout owns.
fn guard(handle: &SandboxHandle, forest: &Forest) -> Result<()> {
    let ours = forest.dir.display().to_string();
    match util::label(handle, "bento.path") {
        Some(owner) if owner != ours => {
            bail!("box '{}' belongs to {owner}, not {ours}", forest.name)
        }
        _ => Ok(()),
    }
}

/// (Re)create the box. Replaces an existing box of this forest.
pub async fn create(forest: &Forest) -> Result<Sandbox> {
    let nix = builder::ensure().await?;
    if let Ok(handle) = Sandbox::get(&forest.name).await {
        guard(&handle, forest)?;
    }
    let home = util::home()?;
    let state = state_dir(forest)?;
    fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;
    let mounts = mounts(forest, &nix, &home, &state)?;
    let env = env(&home, forest)?;
    let spec = spec(&mounts, &env);

    let mut sb = Sandbox::builder(forest.name.clone())
        .image(store::IMAGE)
        .user("0:0")
        .cpus(4)
        .memory(8192)
        .workdir(WORKDIR)
        .replace()
        .label("bento.path", forest.dir.display().to_string())
        .label("bento.spec", spec)
        .envs(env)
        // the builder's daemon is reached via host.microsandbox.internal
        .network(|n| {
            n.policy(NetworkPolicy::from_profiles([
                NetworkProfile::Public,
                NetworkProfile::Host,
            ]))
        });
    for m in mounts {
        sb = sb.volume(m.guest, |b| match m.kind {
            MountKind::Ro => b.bind(&m.host).readonly(),
            MountKind::Rw(q) => b.bind(&m.host).quota(q),
            MountKind::Live(q) if cfg!(target_os = "linux") => b
                .bind(&m.host)
                .quota(q)
                .stat_virtualization(StatVirtualization::Relaxed)
                .host_permissions(HostPermissions::Mirror),
            MountKind::Live(q) => b.bind(&m.host).quota(q),
            MountKind::Disk => b.disk(&m.host).fstype("ext4"),
        });
    }
    eprintln!("box '{}': creating", forest.name);
    let sb = sb
        .create_detached()
        .await
        .with_context(|| format!("creating box '{}'", forest.name))?;
    util::record_dns(&state.join("host-dns"));
    services(&sb).await?;
    // Agents started by hc never pass through `bento opencode`, so the
    // host's opencode credentials must be in the box from creation. The
    // import is idempotent (a fingerprint marker); `bento opencode` still
    // re-runs it when the host's keys change.
    if let Err(e) = agent::import_opencode_credentials(&sb, &home, &state).await {
        eprintln!("bento: importing opencode credentials: {e:#}");
    }
    herdr::register(forest);
    Ok(sb)
}

fn spec(mounts: &[Mount], env: &[(String, String)]) -> String {
    // BENTO_BRIDGE* is excluded: the daemon address and the box's token are
    // per-box runtime state, not config — a rotated token (regenerated
    // bridge.json) or a daemon on another port must not look like a config
    // change (a "box config changed" warning, or a stopped box silently
    // recreated by ensure_running). BENTO_BOX/BENTO_FOREST name the box
    // itself and stay in.
    let env: Vec<_> = env
        .iter()
        .filter(|(key, _)| !key.starts_with("BENTO_BRIDGE"))
        .collect();
    let desc = format!("{mounts:?}{env:?}");
    util::fingerprint(&[store::IMAGE, &builder::PORT.to_string(), &desc])
}

/// A running, connected box for this forest, created or started as needed.
pub async fn ensure_running(forest: &Forest) -> Result<Sandbox> {
    let nix = builder::ensure().await?;
    let handle = match Sandbox::get(&forest.name).await {
        Ok(handle) => handle,
        Err(MicrosandboxError::SandboxNotFound(_)) => return create(forest).await,
        Err(err) => return Err(err).with_context(|| format!("looking up box '{}'", forest.name)),
    };
    guard(&handle, forest)?;
    let state = state_dir(forest)?;
    fs::create_dir_all(&state)?;
    let home = util::home()?;
    let want = spec(&mounts(forest, &nix, &home, &state)?, &env(&home, forest)?);
    let current = util::label(&handle, "bento.spec").as_deref() == Some(want.as_str());
    let sb = match handle.connect().await {
        Ok(sb) => {
            if !current {
                eprintln!("warning: box config changed (bento.yaml, agent config or bento itself); run `bento start` to apply");
            }
            if util::dns_stale(&state.join("host-dns")) {
                eprintln!("warning: host network changed since the box started, so its DNS fails; run `bento start` to fix (restarts the box)");
            }
            sb
        }
        Err(MicrosandboxError::SandboxNotRunning(_)) if !current => {
            // stopped anyway: recreating loses nothing (state is on the host)
            return create(forest).await;
        }
        Err(MicrosandboxError::SandboxNotRunning(_)) => {
            eprintln!("box '{}': starting", forest.name);
            let sb = handle
                .start_detached()
                .await
                .with_context(|| format!("starting box '{}'", forest.name))?;
            util::record_dns(&state.join("host-dns"));
            sb
        }
        Err(err) => {
            return Err(err).with_context(|| format!("connecting to box '{}'", forest.name))
        }
    };
    services(&sb).await?;
    Ok(sb)
}

/// Background processes don't survive stop→start, so the socket forwarder to
/// the builder's daemon and dockerd are (re)started on every connect when not
/// running.
async fn services(sb: &Sandbox) -> Result<()> {
    let script = format!(
        // pid files outlive a stop (/tmp is on the rootfs) and pids repeat
        // across boots, so check the pid's executable, not its cmdline (this
        // script's own cmdline names both)
        r#"running() {{ p=$(cat /tmp/$1.pid 2>/dev/null); case "$(readlink /proc/$p/exe 2>/dev/null)" in */$2*|*/.$2*) true;; *) false;; esac; }}
if ! running nix-fwd socat; then
  rm -f {NIX_SOCK}
  nohup {tools}/bin/socat UNIX-LISTEN:{NIX_SOCK},fork TCP:host.microsandbox.internal:{port} </dev/null >/tmp/nix-fwd.log 2>&1 &
  echo $! > /tmp/nix-fwd.pid
fi
if ! running dockerd dockerd; then
  rm -f {DOCKER_SOCK}
  nohup {tools}/bin/dockerd --data-root {DOCKER_ROOT} </dev/null >/tmp/dockerd.log 2>&1 &
  echo $! > /tmp/dockerd.pid
fi
for i in $(seq 50); do [ -S {NIX_SOCK} ] && break; sleep 0.1; done
[ -S {NIX_SOCK} ] || {{ echo "nix socket forwarder did not start:" >&2; cat /tmp/nix-fwd.log >&2; exit 1; }}
for i in $(seq 100); do [ -S {DOCKER_SOCK} ] && exit 0; sleep 0.1; done
echo "warning: dockerd did not start (log: /tmp/dockerd.log in the box):" >&2; tail -5 /tmp/dockerd.log >&2"#,
        tools = builder::TOOLS,
        port = builder::PORT,
    );
    let out = sb
        .shell(script)
        .await
        .context("starting the box services")?;
    let stderr = out.stderr().unwrap_or_default();
    if !out.status().success {
        bail!("{}", stderr.trim());
    }
    if !stderr.trim().is_empty() {
        eprintln!("{}", stderr.trim());
    }
    Ok(())
}

pub async fn stop(forest: &Forest) -> Result<()> {
    let handle = Sandbox::get(&forest.name)
        .await
        .with_context(|| format!("no box '{}'", forest.name))?;
    guard(&handle, forest)?;
    handle
        .stop()
        .await
        .with_context(|| format!("stopping box '{}'", forest.name))?;
    println!("box '{}': stopped", forest.name);
    Ok(())
}

/// Remove the box and its state dir.
pub async fn rm(forest: &Forest) -> Result<()> {
    if let Ok(handle) = Sandbox::get(&forest.name).await {
        guard(&handle, forest)?;
    }
    remove(&forest.name).await?;
    Ok(())
}

/// Every bento box (sandboxes labeled with the path they belong to), by name.
async fn boxes() -> Result<Vec<(SandboxHandle, String)>> {
    let mut boxes = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = Sandbox::list_with(|l| match cursor.take() {
            Some(c) => l.limit(100).cursor(c),
            None => l.limit(100),
        })
        .await
        .context("listing boxes")?;
        for handle in page.sandboxes {
            if let Some(path) = util::label(&handle, "bento.path") {
                boxes.push((handle, path));
            }
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    boxes.sort_by(|a, b| a.0.name().cmp(b.0.name()));
    Ok(boxes)
}

/// Print the builder with the store's disk use, then every box with its
/// status, disk use and the checkout or forest it belongs to. A status notes
/// why the box or builder is due for a restart: a box's with `bento start`,
/// the builder's happens by itself on the next bento command.
pub async fn ls() -> Result<()> {
    let mut rows = Vec::new();
    if store::backing()?.exists() {
        let status = match builder::status().await? {
            Some((status, Some(stale))) => format!("{status} ({stale})"),
            Some((status, None)) => status,
            None => "absent".into(),
        };
        let size = store::usage().map_or("?".into(), util::human_bytes);
        rows.push((builder::NAME.to_string(), status, size, store::describe()?));
    }
    for (handle, path) in &boxes().await? {
        let mut status = format!("{:?}", handle.status_snapshot()).to_lowercase();
        if status == "running" && util::dns_stale(&box_state_dir(handle.name())?.join("host-dns")) {
            status.push_str(" (network changed)");
        }
        let size = match size(handle.name()).await {
            Ok(bytes) => util::human_bytes(bytes),
            Err(_) => "?".into(),
        };
        let gone = if Path::new(path).exists() {
            ""
        } else {
            " (gone)"
        };
        rows.push((
            handle.name().to_string(),
            status,
            size,
            format!("{path}{gone}"),
        ));
    }
    if rows.is_empty() {
        println!("no boxes");
        return Ok(());
    }
    let name_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(4);
    let status_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(6);
    let size_w = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max(4);
    println!(
        "{:name_w$}  {:status_w$}  {:>size_w$}  PATH",
        "NAME", "STATUS", "SIZE"
    );
    for (name, status, size, path) in rows {
        println!("{name:name_w$}  {status:status_w$}  {size:>size_w$}  {path}");
    }
    Ok(())
}

/// Disk a box takes that `bento rm` frees: msb's sandbox dir (the rootfs
/// layer the box writes) plus its state dir (/workspace root, overlays, agent
/// state). Bound checkouts and the shared store are not counted.
async fn size(name: &str) -> Result<u64> {
    let msb = match Sandbox::get(name).await {
        Ok(handle) => {
            let usage = handle.storage_usage().await?;
            usage
                .allocated_bytes
                .or(usage.logical_bytes)
                .context("sandbox dir scan incomplete")?
        }
        Err(MicrosandboxError::SandboxNotFound(_)) => 0,
        Err(err) => return Err(err).with_context(|| format!("looking up box '{name}'")),
    };
    Ok(msb + util::disk_usage(&box_state_dir(name)?)?)
}

/// Remove every box whose checkout or forest is gone (deleted, or renamed
/// by `forest mv`), with its state dir.
pub async fn prune() -> Result<()> {
    let gone: Vec<(String, String)> = boxes()
        .await?
        .into_iter()
        .filter(|(_, path)| !Path::new(path).exists())
        .map(|(handle, path)| (handle.name().to_string(), path))
        .collect();
    if gone.is_empty() {
        println!("nothing to prune");
    }
    let mut total = Some(0);
    for (name, path) in &gone {
        println!("box '{name}': {path} is gone");
        herdr::unregister(Path::new(path), name);
        let freed = remove(name).await?;
        total = total.zip(freed).map(|(t, f)| t + f);
    }
    if gone.len() > 1 {
        if let Some(total) = total {
            println!("freed {} in total", util::human_bytes(total));
        }
    }
    Ok(())
}

/// Stop a box and archive its agent chats. This comes before removing the
/// sandbox, so a failure leaves the box listed and `prune` can retry.
async fn archive_chats(name: &str) -> Result<()> {
    if let Ok(handle) = Sandbox::get(name).await {
        // stopping an already stopped sandbox fails
        let _ = handle.stop().await;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let dest = util::data_dir()?
        .join("archive")
        .join(format!("{name}-{now}"));
    agent::archive(&box_state_dir(name)?, &dest)
        .with_context(|| format!("archiving the chats of box '{name}', not removing it"))
}

/// Remove a box and its state dir, keeping its chats; returns the bytes
/// freed, or None when they could not be measured (removal goes ahead
/// regardless).
async fn remove(name: &str) -> Result<Option<u64>> {
    archive_chats(name).await?;
    let freed = match size(name).await {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            eprintln!("bento: measuring box '{name}': {e:#}");
            None
        }
    };
    let mut removed = util::remove_sandbox(name).await?;
    if removed {
        println!("box '{name}': removed");
    }
    let dir = box_state_dir(name)?;
    match fs::remove_dir_all(&dir) {
        Ok(()) => {
            println!("removed {}", dir.display());
            removed = true;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", dir.display())),
    }
    if let (true, Some(bytes)) = (removed, freed) {
        println!("box '{name}': freed {}", util::human_bytes(bytes));
    }
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a shadowed git dir's mounts are: every one read-only, guest
    /// path under the git dir, host path the real file.
    fn shadow_pairs(mounts: &[Mount]) -> Vec<(String, PathBuf, bool)> {
        mounts
            .iter()
            .map(|m| {
                let ro = matches!(m.kind, MountKind::Ro);
                (m.guest.clone(), m.host.clone(), ro)
            })
            .collect()
    }

    #[test]
    fn git_shadows_cover_the_host_executable_parts() {
        let base = std::env::temp_dir().join(format!("bento-shadows-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        // a git dir as a linked worktree's main dir looks: config, hooks/,
        // info/, worktrees/<id>/{gitdir,config.worktree}
        let git = base.join("main").join(".git");
        for dir in ["hooks", "info", "worktrees/wt-1", "objects"] {
            fs::create_dir_all(git.join(dir)).unwrap();
        }
        fs::write(git.join("config"), "[core]\n").unwrap();
        fs::write(git.join("worktrees/wt-1/config.worktree"), "[core]\n").unwrap();

        let mounts = git_shadows("/git/x", &git, Some("wt-1"));
        assert_eq!(
            shadow_pairs(&mounts),
            [
                ("/git/x/config".into(), git.join("config"), true),
                ("/git/x/hooks".into(), git.join("hooks"), true),
                ("/git/x/info".into(), git.join("info"), true),
                (
                    "/git/x/worktrees/wt-1/config.worktree".into(),
                    git.join("worktrees/wt-1/config.worktree"),
                    true
                ),
            ]
        );

        // without a config.worktree (the usual case), without it in the list
        fs::remove_file(git.join("worktrees/wt-1/config.worktree")).unwrap();
        let mounts = git_shadows("/git/x", &git, Some("wt-1"));
        assert_eq!(shadow_pairs(&mounts).len(), 3);
        // a main checkout: no worktree id, nothing skipped
        let mounts = git_shadows("/workspace/x/.git", &git, None);
        assert_eq!(
            shadow_pairs(&mounts)[0],
            ("/workspace/x/.git/config".into(), git.join("config"), true)
        );
        assert!(shadow_pairs(&mounts)
            .iter()
            .all(|(g, _, _)| !g.contains("config.worktree")));
        // a bare-ish git dir missing hooks/info: only what exists is bound
        fs::remove_dir_all(git.join("hooks")).unwrap();
        fs::remove_dir_all(git.join("info")).unwrap();
        let mounts = git_shadows("/git/x", &git, Some("wt-1"));
        assert_eq!(shadow_pairs(&mounts).len(), 1);
        fs::remove_dir_all(&base).unwrap();
    }

    /// The shadows are safe to add because git writes config, hooks and
    /// info only for config-ish commands: commits, branch moves and
    /// worktree reads leave them byte-identical (so a read-only bind
    /// changes nothing for them), while `git config` — the thing the
    /// shadows stop — does write.
    #[test]
    fn git_never_writes_config_hooks_or_info_for_normal_work() {
        let base = std::env::temp_dir().join(format!("bento-gitwrite-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::write(
            base.join("gitconfig"),
            "[user]\nname = T\nemail = t@example.com\n[init]\ndefaultBranch = main\n",
        )
        .unwrap();
        let repo = base.join("repo");
        fs::create_dir_all(&repo).unwrap();
        run(&base, &repo, &["init", "-q"]);
        run(
            &base,
            &repo,
            &["commit", "-q", "--allow-empty", "-m", "first"],
        );
        let wt = base.join("wt");
        run(
            &base,
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--no-track",
                "-b",
                "feat",
                &wt.display().to_string(),
            ],
        );
        let parts = |dir: &Path| -> Vec<(String, String)> {
            let mut seen = Vec::new();
            for name in ["config", "hooks", "info"] {
                let path = dir.join(".git").join(name);
                seen.push((
                    name.to_string(),
                    fs::read_to_string(&path).unwrap_or_default(),
                ));
            }
            seen
        };
        let main_before = parts(&repo);
        let wt_dir = fs::read_to_string(wt.join(".git"))
            .unwrap()
            .trim()
            .trim_start_matches("gitdir:")
            .trim()
            .to_string();
        let wt_before = parts(Path::new(&wt_dir));

        // the work git does in a box: commit, move the branch, read worktrees
        run(
            &base,
            &wt,
            &["commit", "-q", "--allow-empty", "-m", "second"],
        );
        run(&base, &wt, &["branch", "-m", "feat", "feat2"]);
        run(&base, &wt, &["worktree", "list"]);
        run(&base, &wt, &["status", "--porcelain"]);

        assert_eq!(
            parts(&repo),
            main_before,
            "main git dir's config/hooks/info changed"
        );
        assert_eq!(
            parts(Path::new(&wt_dir)),
            wt_before,
            "worktree git dir changed"
        );

        // and the write the shadows exist to stop: git config does write
        run(&base, &wt, &["config", "user.name", "Pwned"]);
        assert_ne!(
            parts(&repo),
            main_before,
            "git config must be the blocked write"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    /// Plain git, hermetic against the host's config; asserts success.
    fn run(base: &Path, dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", base.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("running git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn bridge_env_leaves_no_fingerprint_trace() {
        let mounts = [Mount::ro("/nix", "/nix/store")];
        let base = vec![
            ("HOME".into(), "/home/g".into()),
            ("BENTO_BOX".into(), "bento-forest_x".into()),
            ("BENTO_FOREST".into(), "x".into()),
        ];
        let with_bridge = |addr: &str, token: &str| {
            let mut env = base.clone();
            env.push(("BENTO_BRIDGE".into(), addr.into()));
            env.push(("BENTO_BRIDGE_TOKEN".into(), token.into()));
            spec(&mounts, &env)
        };
        // different tokens and ports: same fingerprint
        assert_eq!(
            with_bridge("host.microsandbox.internal:17761", "aa"),
            with_bridge("host.microsandbox.internal:17771", "bb")
        );
        // ... and the same as not passing the bridge vars at all
        assert_eq!(spec(&mounts, &base), with_bridge("x", "y"));
        // a different BENTO_BOX (allowed to stay in) does change it
        let mut other = base.clone();
        other[1] = ("BENTO_BOX".into(), "bento-forest_y".into());
        assert_ne!(spec(&mounts, &base), spec(&mounts, &other));
    }
}
