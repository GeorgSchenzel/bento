//! bento's record of each box, under `$BENTO_HOME/boxes/<name>/`: `box.json`
//! (its address, its `create -e` env, what its last boot applied) and
//! `disk.img`, its data disk. The box itself never sees this dir.

use std::collections::BTreeSet;
use std::fs::File;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::config::Loopback;
use crate::instance::Instance;
use crate::names;
use crate::util;

pub const RECORD: &str = "box.json";
pub const DISK: &str = "disk.img";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// `create` hasn't finished (or was killed: then `unknown`).
    Creating,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoxState {
    pub name: String,
    /// The msb sandbox name (the instance prefix at create time), so `rm`
    /// and `ls` work even when the config doesn't parse.
    pub msb_name: String,
    pub ip: Ipv4Addr,
    /// `create -e`, for the box's life.
    pub env: Vec<(String, String)>,
    pub phase: Phase,
    /// What the last boot applied; None before the first.
    pub boot: Option<Boot>,
    pub ports: Ports,
}

/// The global config's part a boot fixes until the next one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Boot {
    pub config_hash: String,
    /// bento's own env and the global `env`, in order.
    pub env: Vec<(String, String)>,
    /// Read at each exec.
    pub env_files: Vec<(String, PathBuf)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ports {
    /// The union of the workspace's `bento.yaml` ports, as last read.
    pub declared: BTreeSet<u16>,
    /// Published at the last boot.
    pub published: BTreeSet<u16>,
    /// Declared at the last boot, but the host port couldn't be bound.
    pub busy: BTreeSet<u16>,
}

impl BoxState {
    pub fn disk(&self, instance: &Instance) -> PathBuf {
        instance.box_dir(&self.name).join(DISK)
    }
}

pub fn record_path(instance: &Instance, name: &str) -> PathBuf {
    instance.box_dir(name).join(RECORD)
}

/// The record of a box; None when there is no such box.
pub fn load(instance: &Instance, name: &str) -> Result<Option<BoxState>> {
    let path = record_path(instance, name);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let state: BoxState =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(state))
}

pub fn save(instance: &Instance, state: &BoxState) -> Result<()> {
    let json = serde_json::to_vec_pretty(state)?;
    util::write_atomic(&record_path(instance, &state.name), &json)
}

/// Names of every box with a record dir, sorted. Dirs that aren't box names
/// are ignored.
pub fn names(instance: &Instance) -> Result<Vec<String>> {
    let dir = instance.boxes_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            if names::check_box(name).is_ok() && entry.path().is_dir() {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The lowest free address in the range; None when it's full.
pub fn pick_ip(range: &Loopback, used: &BTreeSet<Ipv4Addr>) -> Option<Ipv4Addr> {
    range.hosts().find(|ip| !used.contains(ip))
}

/// Addresses held by this instance's boxes.
pub fn used_ips(instance: &Instance) -> Result<BTreeSet<Ipv4Addr>> {
    let mut used = BTreeSet::new();
    for name in names(instance)? {
        if let Some(s) = load(instance, &name)? {
            used.insert(s.ip);
        }
    }
    Ok(used)
}

/// An flock, held until dropped. One process takes each lock at most once.
pub struct Lock {
    _file: File,
}

fn lock_file(instance: &Instance, name: &str) -> Result<File> {
    let dir = instance.locks_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{name}.lock"));
    File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))
}

impl Lock {
    /// A box's lifecycle lock: create, start, stop, restart and rm hold it;
    /// exec, enter and cp hold it while they make sure the box runs.
    pub fn box_lock(instance: &Instance, name: &str) -> Result<Lock> {
        Self::take(instance, &format!("box-{name}"))
    }

    /// Whether someone holds a box's lock right now.
    pub fn box_busy(instance: &Instance, name: &str) -> Result<bool> {
        let file = lock_file(instance, &format!("box-{name}"))?;
        match file.try_lock() {
            Ok(()) => Ok(false),
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(e)) => Err(e).context("checking a box lock"),
        }
    }

    /// The instance lock: name and address allocation.
    pub fn instance(instance: &Instance) -> Result<Lock> {
        Self::take(instance, "instance")
    }

    pub fn builder(instance: &Instance) -> Result<Lock> {
        Self::take(instance, "builder")
    }

    fn take(instance: &Instance, name: &str) -> Result<Lock> {
        let file = lock_file(instance, name)?;
        file.lock().with_context(|| format!("locking {name}"))?;
        Ok(Lock { _file: file })
    }
}

pub fn remove_dir(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// A scratch instance under the system temp dir; removed on drop.
    pub struct Scratch(pub Instance);

    impl Scratch {
        pub fn new(tag: &str) -> Scratch {
            let root = std::env::temp_dir().join(format!(
                "bento-test-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let home = root.join("home");
            std::fs::create_dir_all(&home).unwrap();
            Scratch(
                Instance::resolve(Some(root.join("bh").as_os_str()), Some(home.as_os_str()))
                    .unwrap(),
            )
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.root.parent().unwrap());
        }
    }

    fn record(name: &str, ip: Ipv4Addr) -> BoxState {
        BoxState {
            name: name.into(),
            msb_name: format!("t-{name}"),
            ip,
            env: vec![("A".into(), "b".into())],
            phase: Phase::Ready,
            boot: None,
            ports: Ports::default(),
        }
    }

    #[test]
    fn records_round_trip_and_list() {
        let s = Scratch::new("records");
        let i = &s.0;
        assert_eq!(load(i, "a").unwrap(), None);
        let a = record("a", Ipv4Addr::new(127, 77, 0, 1));
        save(i, &a).unwrap();
        save(i, &record("b", Ipv4Addr::new(127, 77, 0, 3))).unwrap();
        std::fs::create_dir_all(i.boxes_dir().join("Not_A_Box")).unwrap();
        assert_eq!(load(i, "a").unwrap(), Some(a));
        assert_eq!(names(i).unwrap(), ["a", "b"]);
        let used = used_ips(i).unwrap();
        let range = Loopback::parse("127.77.0.0/24").unwrap();
        assert_eq!(pick_ip(&range, &used), Some(Ipv4Addr::new(127, 77, 0, 2)));
        let _ = OsStr::new("");
    }

    #[test]
    fn a_full_range_has_no_address() {
        let range = Loopback::parse("127.78.0.0/30").unwrap();
        let used: BTreeSet<_> = range.hosts().collect();
        assert_eq!(pick_ip(&range, &used), None);
    }

    #[test]
    fn box_lock_is_exclusive() {
        let s = Scratch::new("lock");
        assert!(!Lock::box_busy(&s.0, "a").unwrap());
        let held = Lock::box_lock(&s.0, "a").unwrap();
        assert!(Lock::box_busy(&s.0, "a").unwrap());
        assert!(!Lock::box_busy(&s.0, "b").unwrap());
        drop(held);
        assert!(!Lock::box_busy(&s.0, "a").unwrap());
    }
}
