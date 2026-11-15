use std::io::Write as _;
use std::path::Path;

use anyhow::{bail, Context as _, Result};
use microsandbox::sandbox::Sandbox;
use microsandbox::{ExecEvent, MicrosandboxError};

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

/// The host's primary DNS servers: what msb forwards a sandbox's DNS to. msb
/// reads them (macOS dynamic store, /etc/resolv.conf on Linux) once, when the
/// sandbox boots, so after a network change a running sandbox keeps asking the
/// old network's servers and name lookups fail. None when they can't be read.
#[cfg(target_os = "linux")]
pub fn host_dns() -> Option<String> {
    let text = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    let servers: Vec<&str> = text
        .lines()
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            (words.next()? == "nameserver").then(|| words.next())?
        })
        .collect();
    (!servers.is_empty()).then(|| servers.join(" "))
}

/// See the Linux `host_dns`.
#[cfg(target_os = "macos")]
pub fn host_dns() -> Option<String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("scutil")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()?
        .write_all(b"show State:/Network/Global/DNS\n")
        .ok()?;
    let out = child.wait_with_output().ok()?;
    // ServerAddresses : <array> {
    //   0 : 10.10.10.1
    // }
    let text = String::from_utf8_lossy(&out.stdout);
    let servers: Vec<&str> = text
        .lines()
        .skip_while(|l| !l.contains("ServerAddresses"))
        .skip(1)
        .take_while(|l| l.trim() != "}")
        .filter_map(|l| l.split_once(" : ").map(|(_, addr)| addr.trim()))
        .collect();
    (!servers.is_empty()).then(|| servers.join(" "))
}

/// Remember the host DNS a sandbox booted with (see `host_dns`).
pub fn record_dns(path: &Path) {
    let _ = match host_dns() {
        Some(dns) => std::fs::write(path, dns),
        None => std::fs::remove_file(path),
    };
}

/// The host DNS changed since the sandbox recorded at `path` booted: its DNS
/// is stale until it is restarted.
pub fn dns_stale(path: &Path) -> bool {
    match (std::fs::read_to_string(path), host_dns()) {
        (Ok(booted), Some(now)) => booted != now,
        _ => false,
    }
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

/// Quote for a POSIX shell: single quotes, with `'` as `'\''`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Write a file through a temp file and a rename, so readers never see half.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("path without a parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("f"),
        std::process::id()
    ));
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_round_trips_through_sh() {
        for s in [
            "plain",
            "a b",
            "it's",
            "$(rm -rf /)",
            "`x`",
            "\\n",
            "\"q\"",
            "'",
            "",
        ] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", sh_quote(s)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), s);
        }
    }

    #[test]
    fn fingerprint_is_stable() {
        // a changed value recreates every builder and box: never change it
        assert_eq!(fingerprint(&["a", "b"]), fingerprint(&["a", "b"]));
        assert_ne!(fingerprint(&["ab"]), fingerprint(&["a", "b"]));
        assert_eq!(fingerprint(&[]), "cbf29ce484222325");
    }
}
