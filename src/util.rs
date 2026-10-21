use std::io::Write as _;
use std::path::{Path, PathBuf};

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

/// Allocated bytes under a dir, like `du -s` (symlinks are not followed, so
/// sparse files count what they occupy); a missing dir is 0.
pub fn disk_usage(path: &Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut total = meta.blocks() * 512;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
            total += disk_usage(&entry?.path())?;
        }
    }
    Ok(total)
}

/// `1.4G`-style size, powers of 1024 like `du -h`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 || size >= 10.0 {
        format!("{size:.0}{}", UNITS[unit])
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
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

/// Host terminal identity and locale for attach sessions; the SDK does not
/// propagate them, and TUIs (claude, opencode) detect terminal features from
/// them. The host's terminfo entry and locale go in as BENTO_* for
/// ATTACH_WRAPPER, which installs / applies them when the box supports them.
pub fn term_env() -> Vec<(String, String)> {
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
    let mut env = vec![("TERM".to_string(), term.clone())];
    for key in ["COLORTERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION"] {
        if let Ok(value) = std::env::var(key) {
            env.push((key.to_string(), value));
        }
    }
    for (key, value) in std::env::vars() {
        if key == "LANG" || key.starts_with("LC_") {
            env.push((format!("BENTO_{key}"), value));
        }
    }
    let terminfo = std::process::Command::new("infocmp")
        .args(["-x", &term])
        .stderr(std::process::Stdio::null())
        .output();
    if let Ok(out) = terminfo {
        if out.status.success() {
            env.push((
                "BENTO_TERMINFO".to_string(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            ));
        }
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

/// Installs the host's terminfo entry when the guest has none for TERM (e.g.
/// xterm-ghostty), falling back to a generic TERM, then execs the command,
/// inside the nearest devenv up from the cwd when there is one (the tree's;
/// the forest root has none). devenv itself finds that root. The host locale is applied in there, where devenv's locale archive
/// is visible: a locale the box lacks becomes C.UTF-8 (built into glibc), or
/// is dropped if it isn't UTF-8.
const ATTACH_WRAPPER: &str = r#"if ! infocmp "$TERM" >/dev/null 2>&1; then
  [ -n "$BENTO_TERMINFO" ] && printf '%s\n' "$BENTO_TERMINFO" | tic -x -o "$HOME/.terminfo" /dev/stdin >/dev/null 2>&1
  infocmp "$TERM" >/dev/null 2>&1 || export TERM=xterm-256color
fi
unset BENTO_TERMINFO
locale='for k in $(compgen -e); do
  case $k in BENTO_LANG|BENTO_LC_*) ;; *) continue ;; esac
  v=${!k}; unset "$k"
  if [ -z "$(LC_ALL=$v bash -c : 2>&1)" ]; then export "${k#BENTO_}=$v"
  else case $v in *[Uu][Tt][Ff]*8*) export "${k#BENTO_}=C.UTF-8" ;; esac
  fi
done
exec "$@"'
d=$PWD
while [ "$d" != / ] && [ ! -f "$d/devenv.nix" ]; do d=$(dirname "$d"); done
[ -f "$d/devenv.nix" ] && exec devenv shell -- bash -c "$locale" bash "$0" "$@"
exec bash -c "$locale" bash "$0" "$@""#;

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
