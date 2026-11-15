//! Where an instance keeps everything: `BENTO_HOME` (unset:
//! `~/.local/share/bento`). msb's own home moves with it (`MSB_HOME`), so two
//! instances never see each other's sandboxes; main sets it before the SDK
//! is touched.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};

/// msb puts its sockets at `<home>/run/sandboxes/<hash>/…`, a fixed 50-byte
/// suffix within a 108-byte `sun_path` (A1, work-bento.md).
pub const MAX_MSB_HOME: usize = 56;

#[derive(Debug, Clone)]
pub struct Instance {
    /// `$BENTO_HOME`, or the default.
    pub root: PathBuf,
    /// `BENTO_HOME` was set: the config is `$BENTO_HOME/config.yaml` and must
    /// set `instance`.
    pub explicit: bool,
    /// The host's HOME.
    pub user_home: PathBuf,
}

impl Instance {
    /// From `BENTO_HOME` and the host HOME. Doesn't touch the filesystem.
    pub fn resolve(bento_home: Option<&OsStr>, user_home: Option<&OsStr>) -> Result<Instance> {
        let user_home = PathBuf::from(user_home.context("HOME is not set")?);
        if !user_home.is_absolute() {
            bail!("HOME must be absolute: {}", user_home.display());
        }
        let (root, explicit) = match bento_home {
            Some(dir) if dir.is_empty() => bail!("BENTO_HOME is set but empty"),
            Some(dir) => {
                let dir = PathBuf::from(dir);
                if !dir.is_absolute() {
                    bail!("BENTO_HOME must be absolute: {}", dir.display());
                }
                (dir, true)
            }
            None => (user_home.join(".local/share/bento"), false),
        };
        let instance = Instance {
            root: normalize(&root),
            explicit,
            user_home,
        };
        Ok(instance)
    }

    /// Fails when msb's home would be too long for its socket paths.
    pub fn check_msb_home(&self) -> Result<()> {
        let len = self.msb_home().as_os_str().len();
        if len > MAX_MSB_HOME {
            bail!(
                "microsandbox's home {} is {len} bytes, at most {MAX_MSB_HOME} work (its socket paths); use a shorter BENTO_HOME",
                self.msb_home().display()
            );
        }
        Ok(())
    }

    pub fn msb_home(&self) -> PathBuf {
        self.root.join("msb")
    }

    pub fn config_path(&self) -> PathBuf {
        if self.explicit {
            self.root.join("config.yaml")
        } else {
            self.user_home.join(".config/bento/config.yaml")
        }
    }

    pub fn boxes_dir(&self) -> PathBuf {
        self.root.join("boxes")
    }

    /// A box's state: its record, its data disk.
    pub fn box_dir(&self, name: &str) -> PathBuf {
        self.boxes_dir().join(name)
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }

    /// The shared nix store's backing dir (`<store>/nix` is `/nix`).
    pub fn store_dir(&self) -> PathBuf {
        self.root.join("store")
    }
}

/// Drop `.` components and a trailing slash, so lengths and prefixes compare
/// as written (`..` is kept: it is the operator's path).
fn normalize(path: &Path) -> PathBuf {
    path.components().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<&OsStr> {
        Some(OsStr::new(s))
    }

    #[test]
    fn default_instance() {
        let i = Instance::resolve(None, os("/home/u")).unwrap();
        assert!(!i.explicit);
        assert_eq!(i.root, Path::new("/home/u/.local/share/bento"));
        assert_eq!(i.msb_home(), Path::new("/home/u/.local/share/bento/msb"));
        assert_eq!(
            i.config_path(),
            Path::new("/home/u/.config/bento/config.yaml")
        );
        i.check_msb_home().unwrap();
    }

    #[test]
    fn bento_home_moves_everything_including_msb() {
        let i = Instance::resolve(os("/tmp/b1/"), os("/home/u")).unwrap();
        assert!(i.explicit);
        assert_eq!(i.config_path(), Path::new("/tmp/b1/config.yaml"));
        for p in [
            i.msb_home(),
            i.boxes_dir(),
            i.box_dir("x"),
            i.locks_dir(),
            i.store_dir(),
        ] {
            assert!(p.starts_with("/tmp/b1"), "{}", p.display());
            assert!(!p.starts_with("/home/u"), "{}", p.display());
        }
        i.check_msb_home().unwrap();
    }

    #[test]
    fn msb_home_length_limit() {
        // "/x" * n + "/msb": the limit is on msb's home, not BENTO_HOME
        let ok = format!("/{}", "a".repeat(MAX_MSB_HOME - 5));
        assert_eq!(ok.len() + 4, MAX_MSB_HOME);
        Instance::resolve(os(&ok), os("/h"))
            .unwrap()
            .check_msb_home()
            .unwrap();
        let long = format!("/{}", "a".repeat(MAX_MSB_HOME - 4));
        let err = Instance::resolve(os(&long), os("/h"))
            .unwrap()
            .check_msb_home()
            .unwrap_err();
        assert!(err.to_string().contains("shorter BENTO_HOME"), "{err}");
        // A1 failed with a 57-byte msb home; this one is 57 too
        let a1 = "/home/georg/projects/forests/exp-agent/scratch/a1/bhm";
        assert_eq!(a1.len() + 4, 57);
        assert!(Instance::resolve(os(a1), os("/h"))
            .unwrap()
            .check_msb_home()
            .is_err());
    }

    #[test]
    fn bad_bento_home() {
        assert!(Instance::resolve(os("rel/dir"), os("/h")).is_err());
        assert!(Instance::resolve(os(""), os("/h")).is_err());
        assert!(Instance::resolve(None, None).is_err());
        assert!(Instance::resolve(None, os("rel")).is_err());
    }
}
