//! The workspace's `bento.yaml` files (contracts.md, `bento.yaml`): read in
//! the box, parsed here. A repo's file must never break a box: whatever is
//! wrong with it is a warning.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use serde::Serialize;
use serde_yaml::Value;

use crate::state::Ports;

/// Prints `<path>\0<content>\0` for `/workspace/bento.yaml` and every
/// `/workspace/<dir>/bento.yaml` whose dir has its own `.git`, at most 64 KiB
/// each. Runs in the box.
pub const READ_SCRIPT: &str = r#"cd /workspace 2>/dev/null || exit 0
for f in bento.yaml */bento.yaml; do
  [ -f "$f" ] || continue
  d=${f%/bento.yaml}
  if [ "$f" != bento.yaml ] && [ ! -e "$d/.git" ]; then continue; fi
  printf '%s\0' "$f"
  head -c 65536 -- "$f" | tr -d '\000'
  printf '\0'
done"#;

#[derive(Debug, Default, PartialEq)]
pub struct Declared {
    pub ports: BTreeSet<u16>,
    pub warnings: Vec<String>,
}

/// Parse READ_SCRIPT's output.
pub fn parse_listing(out: &[u8]) -> Declared {
    let mut declared = Declared::default();
    let mut parts = out.split(|b| *b == 0);
    while let Some(path) = parts.next() {
        if path.is_empty() {
            continue;
        }
        let path = String::from_utf8_lossy(path).into_owned();
        let Some(content) = parts.next() else {
            declared
                .warnings
                .push(format!("/workspace/{path}: cut off while reading"));
            break;
        };
        let text = String::from_utf8_lossy(content);
        if let Some(ports) =
            parse_file(&format!("/workspace/{path}"), &text, &mut declared.warnings)
        {
            declared.ports.extend(ports);
        }
    }
    declared
}

/// One file's ports; None (with a warning) when the file is skipped.
pub fn parse_file(path: &str, text: &str, warnings: &mut Vec<String>) -> Option<Vec<u16>> {
    let value: Value = match serde_yaml::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("{path}: invalid YAML, skipped ({e})"));
            return None;
        }
    };
    let map = match value {
        Value::Null => return Some(vec![]),
        Value::Mapping(m) => m,
        _ => {
            warnings.push(format!("{path}: not a mapping, skipped"));
            return None;
        }
    };
    let mut ports = vec![];
    for (key, value) in map {
        match key.as_str() {
            Some("ports") => match parse_ports(&value) {
                Some(p) => ports = p,
                None => {
                    warnings.push(format!(
                        "{path}: `ports` must be a list of TCP ports (1 to 65535), skipped"
                    ));
                    return None;
                }
            },
            Some(other) => warnings.push(format!("{path}: unknown key `{other}` ignored")),
            None => warnings.push(format!("{path}: a non-string key ignored")),
        }
    }
    Some(ports)
}

fn parse_ports(value: &Value) -> Option<Vec<u16>> {
    match value {
        Value::Null => Some(vec![]),
        Value::Sequence(items) => items
            .iter()
            .map(|v| {
                v.as_u64()
                    .and_then(|n| u16::try_from(n).ok())
                    .filter(|n| *n > 0)
            })
            .collect(),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PortState {
    Published,
    Busy,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PortView {
    pub guest: u16,
    pub address: Option<String>,
    pub state: PortState,
}

/// `bento ports`: every declared port and every port still published from
/// the last boot. Only a running box's published ports have an address.
pub fn port_views(ports: &Ports, ip: Ipv4Addr, running: bool) -> Vec<PortView> {
    let mut all: BTreeSet<u16> = ports.published.clone();
    all.extend(ports.declared.iter().copied());
    all.into_iter()
        .map(|guest| {
            let state = if ports.published.contains(&guest) {
                PortState::Published
            } else if ports.busy.contains(&guest) {
                PortState::Busy
            } else {
                PortState::Pending
            };
            let address =
                (running && state == PortState::Published).then(|| format!("{ip}:{guest}"));
            PortView {
                guest,
                address,
                state,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(files: &[(&str, &str)]) -> Vec<u8> {
        let mut out = vec![];
        for (p, c) in files {
            out.extend_from_slice(p.as_bytes());
            out.push(0);
            out.extend_from_slice(c.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn union_of_files() {
        let d = parse_listing(&listing(&[
            ("bento.yaml", "ports: [5173, 8080]"),
            ("a/bento.yaml", "ports:\n  - 8080\n  - 3000\n"),
            ("b/bento.yaml", ""),
        ]));
        assert_eq!(d.ports, BTreeSet::from([3000, 5173, 8080]));
        assert!(d.warnings.is_empty());
        assert_eq!(parse_listing(b""), Declared::default());
    }

    #[test]
    fn a_bad_file_only_drops_itself() {
        let d = parse_listing(&listing(&[
            ("bento.yaml", "ports: [1]"),
            ("a/bento.yaml", "ports: [2"),
            ("b/bento.yaml", "ports: [3, 70000]"),
            ("c/bento.yaml", "ports: 4"),
            ("d/bento.yaml", "ports: [-5]"),
            ("e/bento.yaml", "ports: ['6']"),
            ("f/bento.yaml", "- 7"),
            ("g/bento.yaml", "ports: [0]"),
        ]));
        assert_eq!(d.ports, BTreeSet::from([1]));
        assert_eq!(d.warnings.len(), 7, "{:?}", d.warnings);
        assert!(d.warnings[0].starts_with("/workspace/a/bento.yaml: invalid YAML"));
    }

    #[test]
    fn unknown_keys_are_ignored_not_the_file() {
        let d = parse_listing(&listing(&[(
            "r/bento.yaml",
            "overlay: [target]\nports: [9000]\nquota_gib: 3\n",
        )]));
        assert_eq!(d.ports, BTreeSet::from([9000]));
        assert_eq!(d.warnings.len(), 2);
        assert!(d.warnings[0].contains("`overlay`"));
    }

    #[test]
    fn hostile_listing() {
        // a path without content, odd bytes, an empty path
        let d = parse_listing(b"\0\0x/bento.yaml");
        assert!(d.ports.is_empty());
        assert_eq!(d.warnings.len(), 1);
        let d = parse_listing(&listing(&[("\u{1b}[2J/bento.yaml", "ports: [\u{fffd}]")]));
        assert!(d.ports.is_empty());
    }

    #[test]
    fn views() {
        let ip = Ipv4Addr::new(127, 77, 0, 5);
        let ports = Ports {
            declared: BTreeSet::from([5173, 8080, 3000]),
            published: BTreeSet::from([5173, 9999]),
            busy: BTreeSet::from([8080, 7777]),
        };
        let v = port_views(&ports, ip, true);
        let got: Vec<_> = v
            .iter()
            .map(|p| (p.guest, p.state, p.address.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                (3000, PortState::Pending, None),
                (5173, PortState::Published, Some("127.77.0.5:5173".into())),
                (8080, PortState::Busy, None),
                // dropped from bento.yaml: still published until the next boot
                (9999, PortState::Published, Some("127.77.0.5:9999".into())),
            ]
        );
        // a busy port that is no longer declared is gone (7777)
        let stopped = port_views(&ports, ip, false);
        assert!(stopped.iter().all(|p| p.address.is_none()));
        assert_eq!(stopped.len(), 4);
    }

    #[test]
    fn read_script_finds_only_repo_files() {
        let dir = std::env::temp_dir().join(format!("bento-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = dir.join("workspace");
        for d in ["repo/.git", "notrepo", "wt", "deep/repo/.git"] {
            std::fs::create_dir_all(ws.join(d)).unwrap();
        }
        std::fs::write(ws.join("wt/.git"), "gitdir: x").unwrap_or(());
        std::fs::write(ws.join("bento.yaml"), "ports: [1]").unwrap();
        std::fs::write(ws.join("repo/bento.yaml"), "ports: [2]").unwrap();
        std::fs::write(ws.join("notrepo/bento.yaml"), "ports: [3]").unwrap();
        std::fs::write(ws.join("wt/bento.yaml"), "ports: [4]").unwrap();
        std::fs::write(ws.join("deep/repo/bento.yaml"), "ports: [5]").unwrap();
        let script = READ_SCRIPT.replace("/workspace", ws.to_str().unwrap());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        let d = parse_listing(&out.stdout);
        assert_eq!(d.ports, BTreeSet::from([1, 2, 4]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
