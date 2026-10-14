//! Boxes: one sandbox per checkout. A box is the pinned nixos/nix image with
//! the shared store mounted read-only; nix goes through the builder's daemon
//! (socat forwarder on /tmp/nix.sock), and the box tools are on PATH.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use microsandbox::sandbox::{Sandbox, SandboxHandle};
use microsandbox::{MicrosandboxError, NetworkPolicy, NetworkProfile};

use crate::agent;
use crate::builder;
use crate::repo::{self, Repo};
use crate::store;
use crate::util;

pub const WORKDIR: &str = "/workspace";
/// Linked-worktree git dir inside the box: the main checkout's git dir is
/// bound here rw (commits write objects/refs/index through it).
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

/// Host-side per-box dir: generated git files and agent state.
pub fn state_dir(repo: &Repo) -> Result<PathBuf> {
    Ok(util::data_dir()?.join("boxes").join(&repo.name))
}

/// Host-side backing tree for overlays: `<checkout>/.bento/ov/<path>`.
fn overlay_root(dir: &Path) -> PathBuf {
    dir.join(".bento/ov")
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

fn mounts(repo: &Repo, nix: &Path, home: &Path, state: &Path) -> Result<Vec<Mount>> {
    let quota = repo.quota_mib;
    let mut mounts = vec![
        Mount::ro("/nix", nix),
        Mount::rw(WORKDIR, &repo.dir, quota),
    ];
    // Box-local copies of platform-specific dirs: the checkout bind is
    // shadowed per entry by its .bento/ov twin, so host (macOS) and box
    // (linux) build artifacts never mix.
    for entry in &repo.overlays {
        let host = overlay_root(&repo.dir).join(entry);
        fs::create_dir_all(&host).with_context(|| format!("creating {}", host.display()))?;
        mounts.push(Mount::rw(format!("{WORKDIR}/{entry}"), host, quota));
    }
    if let Some(wt) = &repo.wt {
        // The worktree's .git file and the worktrees/<id>/gitdir back-pointer
        // hold host paths; inside the box they must reference /workspace and
        // /git, so both are shadowed by generated files.
        let pointer = write(state, "gitdir-pointer", &format!("gitdir: {GUEST_GIT}/worktrees/{}\n", wt.wt_id))?;
        let back = write(state, "gitdir-back", &format!("{WORKDIR}/.git\n"))?;
        mounts.push(Mount::rw(GUEST_GIT, &wt.main_git, quota));
        mounts.push(Mount::ro(format!("{WORKDIR}/.git"), pointer));
        mounts.push(Mount::ro(format!("{GUEST_GIT}/worktrees/{}/gitdir", wt.wt_id), back));
    }
    mounts.push(Mount::ro(home.join(".gitconfig"), write_gitconfig(repo, state, home)?));
    mounts.push(Mount::ro(home.join(".git-exclude"), write(state, "git-exclude", ".bento/\n")?));
    mounts.extend(agent::mounts(home, state, quota)?);
    Ok(mounts)
}

fn write(state: &Path, name: &str, text: &str) -> Result<PathBuf> {
    let path = state.join(name);
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Minimal in-box git config: identity from the host, no signing (host keys
/// and helpers are unavailable in the box), safe.directory for the
/// bind-mounted checkout and git dir, and an excludes file hiding .bento/.
fn write_gitconfig(repo: &Repo, state: &Path, home: &Path) -> Result<PathBuf> {
    let (Some(name), Some(email)) = (
        repo::git(&repo.dir, &["config", "--get", "user.name"]),
        repo::git(&repo.dir, &["config", "--get", "user.email"]),
    ) else {
        bail!("git identity is not configured; set it on the host with:\n  git config --global user.name <name>\n  git config --global user.email <email>");
    };
    let mut text = format!(
        "[user]\n\tname = {name}\n\temail = {email}\n\
         [commit]\n\tgpgsign = false\n\
         [tag]\n\tgpgsign = false\n\
         [core]\n\texcludesFile = {}/.git-exclude\n\
         [safe]\n\tdirectory = {WORKDIR}\n\tdirectory = {GUEST_GIT}\n",
        home.display()
    );
    // git also checks the resolved gitdir for dubious ownership
    if repo.wt.is_some() {
        text.push_str(&format!("\tdirectory = {GUEST_GIT}/worktrees\n"));
    }
    write(state, "gitconfig", &text)
}

/// Names are path-derived; refuse to touch a box another checkout owns.
fn guard(handle: &SandboxHandle, repo: &Repo) -> Result<()> {
    let ours = repo.dir.display().to_string();
    match util::label(handle, "bento.path") {
        Some(owner) if owner != ours => {
            bail!("box '{}' belongs to {owner}, not this checkout", repo.name)
        }
        _ => Ok(()),
    }
}

/// (Re)create the box. Replaces an existing box of this checkout.
pub async fn create(repo: &Repo) -> Result<Sandbox> {
    let nix = builder::ensure().await?;
    if let Ok(handle) = Sandbox::get(&repo.name).await {
        guard(&handle, repo)?;
    }
    let home = util::home()?;
    let state = state_dir(repo)?;
    fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;
    let mounts = mounts(repo, &nix, &home, &state)?;
    let env = env(&home);
    let spec = spec(&mounts, &env);

    let mut sb = Sandbox::builder(repo.name.clone())
        .image(store::IMAGE)
        .user("0:0")
        .cpus(4)
        .memory(8192)
        .workdir(WORKDIR)
        .replace()
        .label("bento.path", repo.dir.display().to_string())
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
    println!("box '{}': creating", repo.name);
    let sb = sb
        .create_detached()
        .await
        .with_context(|| format!("creating box '{}'", repo.name))?;
    forward_nix(&sb).await?;
    Ok(sb)
}

fn spec(mounts: &[Mount], env: &[(String, String)]) -> String {
    let desc = format!("{mounts:?}{env:?}");
    util::fingerprint(&[store::IMAGE, &builder::PORT.to_string(), &desc])
}

/// A running, connected box for this checkout, created or started as needed.
pub async fn ensure_running(repo: &Repo) -> Result<Sandbox> {
    let nix = builder::ensure().await?;
    let handle = match Sandbox::get(&repo.name).await {
        Ok(handle) => handle,
        Err(MicrosandboxError::SandboxNotFound(_)) => return create(repo).await,
        Err(err) => return Err(err).with_context(|| format!("looking up box '{}'", repo.name)),
    };
    guard(&handle, repo)?;
    let state = state_dir(repo)?;
    fs::create_dir_all(&state)?;
    let home = util::home()?;
    let want = spec(&mounts(repo, &nix, &home, &state)?, &env(&home));
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
            return create(repo).await;
        }
        Err(MicrosandboxError::SandboxNotRunning(_)) => {
            println!("box '{}': starting", repo.name);
            handle
                .start_detached()
                .await
                .with_context(|| format!("starting box '{}'", repo.name))?
        }
        Err(err) => return Err(err).with_context(|| format!("connecting to box '{}'", repo.name)),
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

pub async fn stop(repo: &Repo) -> Result<()> {
    let handle = Sandbox::get(&repo.name)
        .await
        .with_context(|| format!("no box '{}'", repo.name))?;
    guard(&handle, repo)?;
    handle
        .stop()
        .await
        .with_context(|| format!("stopping box '{}'", repo.name))?;
    println!("box '{}': stopped", repo.name);
    Ok(())
}

/// Remove the box, its state dir and the overlay tree.
pub async fn clean(repo: &Repo) -> Result<()> {
    if let Ok(handle) = Sandbox::get(&repo.name).await {
        guard(&handle, repo)?;
    }
    if util::remove_sandbox(&repo.name).await? {
        println!("box '{}': removed", repo.name);
    }
    for dir in [state_dir(repo)?, repo.dir.join(".bento")] {
        match fs::remove_dir_all(&dir) {
            Ok(()) => println!("removed {}", dir.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", dir.display())),
        }
    }
    Ok(())
}
