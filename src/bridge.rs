//! The bridge between a box and the host: what an agent in a box uses to
//! ask for things it cannot do itself — forest ops, and (with G1) GPU jobs
//! — over one TCP daemon, `bento serve` (decisions.md §1).
//!
//! This half is the per-box identity: a random token generated once per
//! box, kept in the box's state dir (`boxes/<box>/bridge.json`) and
//! injected into the box env. The daemon authenticates by that token alone
//! and never trusts a forest-naming field in a request; the state dir is
//! host-side, so no other box can read another box's token (R2 §2).

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Default daemon port, next to the builder's 17760.
pub const DEFAULT_PORT: u16 = 17761;

/// How boxes address the host: msb rewrites the gateway to the host's
/// loopback (R2: the only path boxes are guaranteed to reach; the LAN IP
/// is not reachable from a box).
pub const HOST: &str = "host.microsandbox.internal";

/// `boxes/<box>/bridge.json`: the box's bridge identity. `port` is fixed
/// at creation so a later bento call without `BENTO_BRIDGE_PORT` set (hc's
/// via, `bento enter`) still tells the box the same daemon address;
/// `machine` is the hc machine name (decisions §4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxInfo {
    pub token: String,
    #[serde(rename = "box")]
    pub box_name: String,
    pub path: String,
    pub port: u16,
    pub machine: String,
}

/// The bridge port: `BENTO_BRIDGE_PORT`, else the default. A bad value is
/// an error, not a silent fallback — a typo'd port would point boxes at
/// nothing.
pub fn port_from_env() -> Result<u16> {
    match std::env::var("BENTO_BRIDGE_PORT") {
        Ok(v) => v
            .parse()
            .with_context(|| format!("bad BENTO_BRIDGE_PORT '{v}'")),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_PORT),
        Err(e) => Err(e).context("reading BENTO_BRIDGE_PORT"),
    }
}

/// The box's bridge.json, read or created. The token is fixed once per box
/// and reused, so it survives box recreation (`bento start`) without the
/// env plumbing changing under a running setup.
pub fn ensure_box_info(state: &Path, box_name: &str, dir: &Path) -> Result<BoxInfo> {
    let path = state.join("bridge.json");
    if let Ok(text) = fs::read_to_string(&path) {
        if let Ok(info) = serde_json::from_str(&text) {
            return Ok(info);
        }
        // unreadable or corrupt: fall through and regenerate (a fresh token
        // invalidates nothing — the daemon reads the same file)
    }
    let info = BoxInfo {
        token: random_token().context("generating the bridge token")?,
        box_name: box_name.to_string(),
        path: dir.display().to_string(),
        port: port_from_env()?,
        machine: machine_name(&dir_name(dir)),
    };
    write_secret(&path, &serde_json::to_vec_pretty(&info)?)?;
    Ok(info)
}

/// A fresh 128-bit token, hex, straight from the kernel.
fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Write a file no one else can read: mode 0600 at open (never a window
/// with looser permissions), via a temp file + rename.
fn write_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The dir's name when it is a workforest (`<root>/forests/<name>` with a
/// `repos/` sibling — forest.rs's own rule), else None.
pub fn workforest_name(dir: &Path) -> Option<String> {
    let forests = dir.parent()?;
    if forests.file_name()? != "forests" || !forests.parent()?.join("repos").is_dir() {
        return None;
    }
    Some(dir.file_name()?.to_string_lossy().into_owned())
}

/// The hc machine name for a forest dir name: hc's `clean_name` rules
/// (herdr-core addr.rs) — lowercase, chars outside `[a-z0-9-_.]` become
/// `-`, no leading non-alphanumeric, at most 32 chars. Where hc would
/// fall back to `local` because the result is shaped like a terminal id
/// (`t<digits>`), break the shape instead: `local` would collide across
/// boxes, `t12-` stays theirs alone.
pub fn machine_name(dir_name: &str) -> String {
    let mapped: String = dir_name
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut name: String = mapped
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .chars()
        .take(32)
        .collect();
    if name.is_empty() {
        return "box".into();
    }
    if id_shaped(&name) {
        if name.len() >= 32 {
            name.truncate(31);
        }
        name.push('-');
    }
    name
}

/// hc's rule: a name that could be a terminal id (`t`, then digits with no
/// leading zero) must not be used as a machine name.
fn id_shaped(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('t') else {
        return false;
    };
    let mut chars = rest.chars();
    matches!(chars.next(), Some('1'..='9')) && chars.all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bento-bridge-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn machine_names_follow_hc_rules() {
        assert_eq!(machine_name("spike-F2a-bridge"), "spike-f2a-bridge");
        assert_eq!(machine_name("exp-agent"), "exp-agent");
        assert_eq!(machine_name("Can's MacBook Pro"), "can-s-macbook-pro");
        // capped at hc's 32 chars
        assert_eq!(machine_name(&"a".repeat(40)).len(), 32);
        assert_eq!(machine_name(&"a".repeat(40)), "a".repeat(32));
        // never shaped like a terminal id (t<digits>)
        assert_eq!(machine_name("t12"), "t12-");
        let t = machine_name(&format!("t{}", "1".repeat(40)));
        assert_eq!(t.len(), 32);
        assert!(t.ends_with('-') && !id_shaped(&t));
        // things hc would degrade to "local" keep a name of their own
        assert_eq!(machine_name("--"), "box");
        assert_eq!(machine_name("..."), "box");
    }

    #[test]
    fn workforest_needs_the_layout() {
        let root = tempdir("wf");
        fs::create_dir_all(root.join("repos")).unwrap();
        let dir = root.join("forests").join("thing");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(workforest_name(&dir).as_deref(), Some("thing"));
        // no repos/ sibling: not a forest root
        let other = tempdir("wf2").join("forests").join("thing");
        fs::create_dir_all(&other).unwrap();
        assert_eq!(workforest_name(&other), None);
        // a checkout dir is not a workforest
        assert_eq!(workforest_name(&root.join("repos").join("bento")), None);
    }

    #[test]
    fn box_info_is_created_once_and_secret() {
        use std::os::unix::fs::PermissionsExt as _;
        let state = tempdir("state");
        let dir = state.join("tree");
        fs::create_dir_all(&dir).unwrap();
        let first = ensure_box_info(&state, "bento-test", &dir).unwrap();
        assert_eq!(first.token.len(), 32);
        assert!(first.token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(first.box_name, "bento-test");
        assert_eq!(first.path, dir.display().to_string());
        assert_eq!(first.port, DEFAULT_PORT);
        assert_eq!(first.machine, "tree");
        // mode 0600
        let meta = fs::metadata(state.join("bridge.json")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        // second read returns the same token (no regeneration)
        let second = ensure_box_info(&state, "bento-test", &dir).unwrap();
        assert_eq!(first.token, second.token);
        // a corrupt file is regenerated, not fatal
        fs::write(state.join("bridge.json"), "not json").unwrap();
        let third = ensure_box_info(&state, "bento-test", &dir).unwrap();
        assert_eq!(third.token.len(), 32);
        assert_ne!(third.token, second.token);
    }
}
