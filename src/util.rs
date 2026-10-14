use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use microsandbox::sandbox::Sandbox;
use microsandbox::{ExecEvent, MicrosandboxError};

/// Host HOME. The box uses the same path as its HOME so absolute paths in
/// host agent config (claude's plugin registry) stay valid inside.
pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// `~/.local/share/bento`: store image, per-box state, host lock.
pub fn data_dir() -> Result<PathBuf> {
    let dir = home()?.join(".local/share/bento");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// FNV-1a: a stable fingerprint for sandbox labels (std's hasher may change
/// between Rust releases; a changed fingerprint recreates a sandbox).
pub fn fingerprint(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for part in parts {
        for b in part.bytes().chain([0]) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    format!("{h:016x}")
}

/// Host terminal identity for attach sessions; the SDK does not propagate
/// TERM, and TUIs (claude, opencode) need it.
pub fn term_env() -> Vec<(String, String)> {
    let mut env = vec![(
        "TERM".to_string(),
        std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
    )];
    if let Ok(colorterm) = std::env::var("COLORTERM") {
        env.push(("COLORTERM".to_string(), colorterm));
    }
    env
}

/// Label value of an existing sandbox, if any.
pub fn label(handle: &microsandbox::sandbox::SandboxHandle, key: &str) -> Option<String> {
    handle.config().ok()?.spec.labels.get(key).cloned()
}

/// Stop and remove a sandbox; a missing one is fine.
pub async fn remove_sandbox(name: &str) -> Result<bool> {
    let handle = match Sandbox::get(name).await {
        Ok(handle) => handle,
        Err(MicrosandboxError::SandboxNotFound(_)) => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("looking up sandbox '{name}'")),
    };
    // stopping an already stopped sandbox fails; removal is what matters
    let _ = handle.stop().await;
    handle
        .remove()
        .await
        .with_context(|| format!("removing sandbox '{name}'"))?;
    Ok(true)
}

/// Run a script in a sandbox, forwarding its output live to stderr; fails on
/// a nonzero exit.
pub async fn run_streamed(sb: &Sandbox, what: &str, script: String) -> Result<()> {
    let mut handle = sb
        .shell_stream(script)
        .await
        .with_context(|| format!("{what}: starting"))?;
    let mut err = std::io::stderr();
    while let Some(event) = handle.recv().await {
        match event {
            ExecEvent::Stdout(b) | ExecEvent::Stderr(b) => {
                let _ = err.write_all(&b);
            }
            ExecEvent::Exited { code } if code != 0 => bail!("{what}: exit code {code}"),
            ExecEvent::Exited { .. } => return Ok(()),
            ExecEvent::Failed(f) => bail!("{what}: {f:?}"),
            _ => {}
        }
    }
    bail!("{what}: lost connection to the sandbox")
}

/// Falls back to a generic TERM when the guest has no terminfo for the
/// host's (e.g. a new terminal emulator), then execs the command, inside the
/// checkout's devenv shell when there is one.
const ATTACH_WRAPPER: &str = r#"infocmp "$TERM" >/dev/null 2>&1 || export TERM=xterm-256color
[ -f devenv.nix ] && exec devenv shell -- "$0" "$@"
exec "$0" "$@""#;

/// Attach an interactive command; returns the process exit code.
pub async fn attach(
    sb: &Sandbox,
    cmd: &str,
    args: &[String],
    cwd: &str,
    envs: Vec<(String, String)>,
) -> Result<u8> {
    let argv = ["-c", ATTACH_WRAPPER, cmd]
        .into_iter()
        .map(String::from)
        .chain(args.iter().cloned());
    let code = sb
        .attach_with("bash", |a| a.args(argv).cwd(cwd).envs(envs))
        .await
        .with_context(|| format!("attaching {cmd}"))?;
    // negative: detached with the detach keys
    Ok(if code < 0 { 0 } else { code as u8 })
}
