//! Boxes: one sandbox per forest (a single checkout is a forest of one).
//! A box is the pinned nixos/nix image with the shared store mounted
//! read-only; nix goes through the builder's daemon (socat forwarder on
//! /tmp/nix.sock), and the box tools are on PATH.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use microsandbox::sandbox::{Sandbox, SandboxHandle};
use microsandbox::{MicrosandboxError, NetworkPolicy, NetworkProfile};

use crate::agent;
use crate::builder;
use crate::forest::{self, Forest, WORKDIR};
use crate::store;
use crate::util;

/// Linked-worktree git dirs inside the box: each tree's main git dir is
/// bound rw at /git/<tree> (commits write objects/refs/index through it).
const GUEST_GIT: &str = "/git";
const NIX_SOCK: &str = "/tmp/nix.sock";

/// One bind mount. Writable ones always carry a quota: msb's default is
/// 4 GiB of guest-added bytes, past which writes fail with ENOSPC.
#[derive(Debug)]
pub struct Mount {
    guest: String,
    host: PathBuf,
    quota: Option<u32>,
}

impl Mount {
    pub fn ro(guest: impl AsRef<Path>, host: impl Into<PathBuf>) -> Self {
        Mount {
            guest: guest.as_ref().display().to_string(),
            host: host.into(),
            quota: None,
        }
    }

    pub fn rw(guest: impl AsRef<Path>, host: impl Into<PathBuf>, quota_mib: u32) -> Self {
        Mount {
            guest: guest.as_ref().display().to_string(),
            host: host.into(),
            quota: Some(quota_mib),
        }
    }
}

/// Host-side per-box dir: the /workspace root, overlays, generated git
/// files and agent state.
pub fn state_dir(forest: &Forest) -> Result<PathBuf> {
    box_state_dir(&forest.name)
}

fn box_state_dir(name: &str) -> Result<PathBuf> {
    Ok(util::data_dir()?.join("boxes").join(name))
}

fn env(home: &Path) -> Vec<(String, String)> {
    let mut env = vec![
        ("HOME".into(), home.display().to_string()),
        ("NIX_REMOTE".into(), format!("unix://{NIX_SOCK}")),
        (
            "NIX_CONFIG".into(),
            "experimental-features = nix-command flakes".into(),
        ),
        ("TERMINFO_DIRS".into(), format!("{}/share/terminfo", builder::TOOLS)),
        (
            "PATH".into(),
            format!("{}/bin:/nix/var/nix/profiles/default/bin:/usr/bin:/bin", builder::TOOLS),
        ),
    ];
    env.extend(agent::sandbox_env(home));
    env
}

fn mounts(forest: &Forest, nix: &Path, home: &Path, state: &Path) -> Result<Vec<Mount>> {
    // /workspace is box-owned (the trees are bound into it), so the combined
    // devenv and anything written at the forest root stay off the host forest
    let root = state.join("root");
    fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    let mut mounts = vec![
        Mount::ro("/nix", nix),
        Mount::rw(WORKDIR, &root, forest.quota_mib()),
    ];
    mounts.extend(devenv(forest, &root)?);
    for tree in &forest.trees {
        let quota = tree.quota_mib;
        let guest = format!("{WORKDIR}/{}", tree.name);
        mounts.push(Mount::rw(&guest, &tree.dir, quota));
        // Box-local copies of platform-specific dirs: the tree bind is
        // shadowed per entry by a twin in the state dir, so host (macOS) and
        // box (linux) build artifacts never mix.
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
            let back = write(state, &format!("gitdir-back-{}", tree.name), &format!("{guest}/.git\n"))?;
            mounts.push(Mount::rw(&git, &wt.main_git, quota));
            mounts.push(Mount::ro(format!("{guest}/.git"), pointer));
            mounts.push(Mount::ro(format!("{git}/worktrees/{}/gitdir", wt.wt_id), back));
        }
    }
    mounts.push(Mount::ro(home.join(".gitconfig"), write_gitconfig(forest, state)?));
    mounts.extend(agent::mounts(home, state, forest.quota_mib())?);
    Ok(mounts)
}

/// The combined devenv at /workspace: imports every tree's devenv, so a
/// command started at the forest root gets all their tools (in a tree, the
/// tree's own devenv is nearer and wins). A `devenv.nix` / `devenv.yaml` in
/// the host forest dir is layered in / replaces the generated one, e.g. to
/// resolve conflicting options with `lib.mkForce`.
fn devenv(forest: &Forest, root: &Path) -> Result<Vec<Mount>> {
    let imports: Vec<&str> = forest
        .trees
        .iter()
        .filter(|t| t.dir.join("devenv.nix").is_file())
        .map(|t| t.name.as_str())
        .collect();
    let nix = root.join("devenv.nix");
    let yaml = root.join("devenv.yaml");
    if imports.is_empty() {
        for path in [&nix, &yaml] {
            match fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("removing {}", path.display()))
                }
                _ => {}
            }
        }
        return Ok(vec![]);
    }
    let mut text = String::from("# generated by bento: the forest's devenvs combined\nimports:\n");
    for name in &imports {
        text.push_str(&format!("  - ./{name}\n"));
    }
    fs::write(&yaml, text).with_context(|| format!("writing {}", yaml.display()))?;
    fs::write(&nix, "{ ... }: { }\n").with_context(|| format!("writing {}", nix.display()))?;
    let mut mounts = Vec::new();
    for name in ["devenv.nix", "devenv.yaml"] {
        let own = forest.dir.join(name);
        // a single checkout's own devenv is its tree's, not a forest layer
        if !forest.trees.iter().any(|t| t.dir == forest.dir) && own.is_file() {
            mounts.push(Mount::ro(format!("{WORKDIR}/{name}"), own));
        }
    }
    Ok(mounts)
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
    let env = env(&home);
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
        sb = sb.volume(m.guest, |b| match m.quota {
            Some(q) => b.bind(&m.host).quota(q),
            None => b.bind(&m.host).readonly(),
        });
    }
    println!("box '{}': creating", forest.name);
    let sb = sb
        .create_detached()
        .await
        .with_context(|| format!("creating box '{}'", forest.name))?;
    forward_nix(&sb).await?;
    Ok(sb)
}

fn spec(mounts: &[Mount], env: &[(String, String)]) -> String {
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
    let want = spec(&mounts(forest, &nix, &home, &state)?, &env(&home));
    let current = util::label(&handle, "bento.spec").as_deref() == Some(want.as_str());
    let sb = match handle.connect().await {
        Ok(sb) => {
            if !current {
                println!("warning: box config changed (bento.yaml, agent config or bento itself); run `bento start` to apply");
            }
            sb
        }
        Err(MicrosandboxError::SandboxNotRunning(_)) if !current => {
            // stopped anyway: recreating loses nothing (state is on the host)
            return create(forest).await;
        }
        Err(MicrosandboxError::SandboxNotRunning(_)) => {
            println!("box '{}': starting", forest.name);
            handle
                .start_detached()
                .await
                .with_context(|| format!("starting box '{}'", forest.name))?
        }
        Err(err) => return Err(err).with_context(|| format!("connecting to box '{}'", forest.name)),
    };
    forward_nix(&sb).await?;
    Ok(sb)
}

/// Background processes don't survive stop→start, so the socket forwarder to
/// the builder's daemon is (re)started on every connect when not running.
async fn forward_nix(sb: &Sandbox) -> Result<()> {
    let script = format!(
        r#"p=$(cat /tmp/nix-fwd.pid 2>/dev/null)
if [ -n "$p" ] && grep -qa socat /proc/$p/cmdline 2>/dev/null; then exit 0; fi
rm -f {NIX_SOCK}
nohup {tools}/bin/socat UNIX-LISTEN:{NIX_SOCK},fork TCP:host.microsandbox.internal:{port} </dev/null >/tmp/nix-fwd.log 2>&1 &
echo $! > /tmp/nix-fwd.pid
for i in $(seq 50); do [ -S {NIX_SOCK} ] && exit 0; sleep 0.1; done
echo "nix socket forwarder did not start:" >&2; cat /tmp/nix-fwd.log >&2; exit 1"#,
        tools = builder::TOOLS,
        port = builder::PORT,
    );
    let out = sb.shell(script).await.context("starting the nix forwarder")?;
    if !out.status().success {
        bail!("{}", out.stderr().unwrap_or_default().trim());
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
    remove(&forest.name).await
}

/// Remove every box whose checkout or forest is gone (deleted, or renamed
/// by `forest mv`), with its state dir.
pub async fn prune() -> Result<()> {
    let mut gone = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = Sandbox::list_with(|l| match cursor.take() {
            Some(c) => l.limit(100).cursor(c),
            None => l.limit(100),
        })
        .await
        .context("listing boxes")?;
        for handle in &page.sandboxes {
            if let Some(path) = util::label(handle, "bento.path") {
                if !Path::new(&path).exists() {
                    gone.push((handle.name().to_string(), path));
                }
            }
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    if gone.is_empty() {
        println!("nothing to prune");
    }
    for (name, path) in gone {
        println!("box '{name}': {path} is gone");
        remove(&name).await?;
    }
    Ok(())
}

async fn remove(name: &str) -> Result<()> {
    if util::remove_sandbox(name).await? {
        println!("box '{name}': removed");
    }
    let dir = box_state_dir(name)?;
    match fs::remove_dir_all(&dir) {
        Ok(()) => println!("removed {}", dir.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", dir.display())),
    }
    Ok(())
}
