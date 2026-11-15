//! `bento cp` (docker cp's semantics) and `copy` entries: tar archives
//! between the host and the box. Archives from the box are untrusted: they
//! are unpacked into a fresh dir, entry by entry, never through a link, and
//! only regular files, dirs and symlinks (kept as links, never followed).

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context as _, Result};

use crate::fail;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    Box { name: String, path: String },
    Host(PathBuf),
}

/// A box path when a `:` comes before any `/` (`./a:b` is a host file).
pub fn parse_operand(arg: &str) -> Operand {
    match (arg.find(':'), arg.find('/')) {
        (Some(c), slash) if slash.is_none_or(|s| c < s) => Operand::Box {
            name: arg[..c].to_string(),
            path: arg[c + 1..].to_string(),
        },
        _ => Operand::Host(PathBuf::from(arg)),
    }
}

/// What a path is on either side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    /// A file, a link or anything else that exists.
    File,
    /// Missing, its parent is a dir.
    Missing,
    /// Missing, and so is its parent.
    NoParent,
}

pub fn parse_kind(s: &str) -> Result<Kind> {
    Ok(match s.trim() {
        "dir" => Kind::Dir,
        "file" => Kind::File,
        "missing" => Kind::Missing,
        "noparent" => Kind::NoParent,
        other => bail!("unexpected answer from the box: {other:?}"),
    })
}

pub fn host_kind(path: &Path) -> Kind {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => Kind::Dir,
        Ok(_) => Kind::File,
        Err(_) => match path.parent() {
            Some(p) if p.as_os_str().is_empty() || p.is_dir() => Kind::Missing,
            _ => Kind::NoParent,
        },
    }
}

/// Where a copy lands: the archive's entries go into `dir`; the source's top
/// entry is named `name` there (None: the source dir's contents go into
/// `dir` itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub dir: String,
    pub name: Option<String>,
}

/// docker cp's rules. `src` is the source path as given (a trailing `/.`
/// means "the contents"), `src_kind` must exist; `dest` as given.
pub fn place(src: &str, src_kind: Kind, dest: &str, dest_kind: Kind) -> Result<Placement> {
    let contents = src.ends_with("/.") || src == ".";
    let src_base = base_name(src);
    let dest_trailing = dest.ends_with('/');
    let dest_dir = parent_of(dest);
    let dest_base = base_name(dest);
    let err = |m: String| Err(fail::fail(fail::OTHER, m));
    match (src_kind, dest_kind) {
        (Kind::Missing | Kind::NoParent, _) => err(format!("{src}: no such file or directory")),
        (_, Kind::NoParent) => err(format!("{dest}: its parent dir doesn't exist")),
        (Kind::File, Kind::Dir) => Ok(Placement {
            dir: dest.to_string(),
            name: Some(src_base),
        }),
        (Kind::File, Kind::File) => Ok(Placement {
            dir: dest_dir,
            name: Some(dest_base),
        }),
        (Kind::File, Kind::Missing) if dest_trailing => err(format!(
            "{dest}: no such dir (a trailing / needs an existing dir)"
        )),
        (Kind::File, Kind::Missing) => Ok(Placement {
            dir: dest_dir,
            name: Some(dest_base),
        }),
        (Kind::Dir, Kind::File) => err(format!("{dest}: can't copy a dir onto a file")),
        (Kind::Dir, Kind::Missing) => Ok(Placement {
            dir: dest_dir,
            name: Some(dest_base),
        }),
        (Kind::Dir, Kind::Dir) if contents => Ok(Placement {
            dir: dest.to_string(),
            name: None,
        }),
        (Kind::Dir, Kind::Dir) => Ok(Placement {
            dir: dest.to_string(),
            name: Some(src_base),
        }),
    }
}

/// The last component, ignoring trailing slashes and a trailing `/.`.
pub fn base_name(p: &str) -> String {
    let t = p.trim_end_matches("/.").trim_end_matches('/');
    match t.rsplit_once('/') {
        Some((_, b)) => b.to_string(),
        None => t.to_string(),
    }
}

/// The dir a path is in (`.` for a bare name, `/` for a top-level one).
pub fn parent_of(p: &str) -> String {
    let t = p.trim_end_matches('/');
    match t.rsplit_once('/') {
        Some(("", _)) => "/".to_string(),
        Some((d, _)) => d.to_string(),
        None => ".".to_string(),
    }
}

/// The dir and name to archive in the box for `src`: (`dir`, `name`), with
/// name `.` for a dir's contents.
pub fn archive_root(src: &str) -> (String, String) {
    if src.ends_with("/.") || src == "/" {
        (
            src.trim_end_matches("/.").to_string().max("/".into()),
            ".".into(),
        )
    } else {
        (parent_of(src), base_name(src))
    }
}

// ---------------------------------------------------------------- host → box

/// Write an archive of host path `src` (not following its links) with its
/// top entry named `name` (None: the dir's contents at the archive's root).
/// Owners are root, as everything bento writes in the box.
pub fn write_archive(src: &Path, name: Option<&str>, out: impl Write) -> Result<()> {
    let mut b = tar::Builder::new(out);
    b.follow_symlinks(false);
    match name {
        Some(name) => add_tree(&mut b, src, Path::new(name))?,
        None => {
            for entry in sorted_dir(src)? {
                let rel = PathBuf::from(entry.file_name().unwrap());
                add_tree(&mut b, &entry, &rel)?;
            }
        }
    }
    b.finish().context("writing the archive")?;
    Ok(())
}

/// Like write_archive, but the top entry is `src`'s target when it is a link
/// (for `copy` entries: home-manager's files are links into its store).
pub fn write_archive_following_top(src: &Path, name: &str, out: impl Write) -> Result<()> {
    let real = fs::canonicalize(src).with_context(|| format!("{}", src.display()))?;
    write_archive(&real, Some(name), out)
}

fn sorted_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    v.sort();
    Ok(v)
}

fn add_tree<W: Write>(b: &mut tar::Builder<W>, path: &Path, rel: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("{}", path.display()))?;
    let mut h = tar::Header::new_gnu();
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs()),
    );
    h.set_mode(meta.permissions().mode() & 0o7777);
    let ft = meta.file_type();
    if ft.is_dir() {
        h.set_entry_type(tar::EntryType::Directory);
        h.set_size(0);
        b.append_data(&mut h, rel, std::io::empty())?;
        for child in sorted_dir(path)? {
            let child_rel = rel.join(child.file_name().unwrap());
            add_tree(b, &child, &child_rel)?;
        }
    } else if ft.is_symlink() {
        let target = fs::read_link(path)?;
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, rel, target)?;
    } else if ft.is_file() {
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(meta.len());
        let f = fs::File::open(path).with_context(|| format!("{}", path.display()))?;
        b.append_data(&mut h, rel, f)?;
    } else {
        eprintln!(
            "bento: warning: {}: not a file, dir or link, skipped",
            path.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- box → host

/// Unpack an archive from the box into host dir `dest_dir`, its top entry
/// (`top`, or every entry at the root when `top` is None) landing at
/// `dest_dir/<name>` (None: merged into `dest_dir`).
///
/// Everything is first unpacked into a new dir next to the target, where
/// nothing exists but what this archive made: each entry is created fresh
/// (`O_EXCL|O_NOFOLLOW`), and refused when any of its parents is a link.
/// Then the result is moved into place.
pub fn unpack(archive: impl Read, top: &str, dest_dir: &Path, name: Option<&str>) -> Result<()> {
    let stage = staging_dir(dest_dir)?;
    let res = unpack_into(archive, &stage).and_then(|()| {
        let src = if top == "." {
            stage.clone()
        } else {
            stage.join(top)
        };
        if fs::symlink_metadata(&src).is_err() {
            bail!("the archive from the box holds no {top}");
        }
        match name {
            Some(n) => place_entry(&src, &dest_dir.join(n)),
            None => merge_dir(&src, dest_dir),
        }
    });
    let _ = fs::remove_dir_all(&stage);
    res
}

fn staging_dir(dest_dir: &Path) -> Result<PathBuf> {
    for i in 0..100u32 {
        let p = dest_dir.join(format!(".bento-cp-{}-{i}", std::process::id()));
        match fs::create_dir(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", p.display())),
        }
    }
    bail!("can't create a staging dir in {}", dest_dir.display())
}

/// A relative path made only of normal components.
fn clean_rel(p: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            _ => bail!("unsafe path in the archive: {}", p.display()),
        }
    }
    Ok(out)
}

/// No component of `rel` below `root`, except maybe the last, is a link
/// or a non-dir.
fn parents_are_dirs(root: &Path, rel: &Path) -> Result<()> {
    let mut cur = root.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for c in &comps[..comps.len().saturating_sub(1)] {
        cur.push(c);
        let m = fs::symlink_metadata(&cur)
            .with_context(|| format!("archive entry under missing {}", cur.display()))?;
        if !m.is_dir() {
            bail!("archive entry under a link or file: {}", cur.display());
        }
    }
    Ok(())
}

fn unpack_into(archive: impl Read, root: &Path) -> Result<()> {
    let mut ar = tar::Archive::new(archive);
    for entry in ar.entries().context("reading the archive")? {
        let mut entry = entry.context("reading the archive")?;
        let raw = entry
            .path()
            .context("an archive entry's path")?
            .into_owned();
        let rel = clean_rel(&raw)?;
        if rel.as_os_str().is_empty() {
            continue; // the root itself (`.`)
        }
        parents_are_dirs(root, &rel)?;
        let path = root.join(&rel);
        let mode = entry.header().mode().unwrap_or(0o644) & 0o777;
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                match fs::symlink_metadata(&path) {
                    Ok(m) if m.is_dir() => {}
                    Ok(_) => bail!(
                        "archive entry replaces a file with a dir: {}",
                        rel.display()
                    ),
                    Err(_) => fs::create_dir(&path)
                        .with_context(|| format!("creating {}", path.display()))?,
                }
                fs::set_permissions(&path, fs::Permissions::from_mode(mode | 0o700))?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if fs::symlink_metadata(&path).is_ok() {
                    fs::remove_file(&path)
                        .with_context(|| format!("archive entry twice: {}", rel.display()))?;
                }
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .mode(mode)
                    .open(&path)
                    .with_context(|| format!("creating {}", path.display()))?;
                std::io::copy(&mut entry, &mut f)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
            tar::EntryType::Symlink => {
                let target = entry
                    .link_name()?
                    .context("a link without a target")?
                    .into_owned();
                if fs::symlink_metadata(&path).is_ok() {
                    bail!("archive entry twice: {}", rel.display());
                }
                std::os::unix::fs::symlink(&target, &path)
                    .with_context(|| format!("creating {}", path.display()))?;
            }
            // GNU long names and pax headers are folded into entries by the
            // tar crate; anything else (hard links, devices, fifos) is skipped
            other => eprintln!(
                "bento: warning: {}: {other:?} entries aren't copied",
                rel.display()
            ),
        }
    }
    Ok(())
}

/// Move a staged entry to `dest`: a new name, or over a file, or merged into
/// a dir. Never follows a link at `dest`.
fn place_entry(src: &Path, dest: &Path) -> Result<()> {
    let sm = fs::symlink_metadata(src)?;
    match fs::symlink_metadata(dest) {
        Err(_) => fs::rename(src, dest).with_context(|| format!("writing {}", dest.display())),
        Ok(dm) if sm.is_dir() && dm.is_dir() => merge_dir(src, dest),
        Ok(dm) if sm.is_dir() => bail!(
            "{}: can't copy a dir onto a {}",
            dest.display(),
            if dm.file_type().is_symlink() {
                "link"
            } else {
                "file"
            }
        ),
        Ok(dm) if dm.is_dir() => bail!("{}: can't copy a file onto a dir", dest.display()),
        Ok(_) => fs::rename(src, dest).with_context(|| format!("writing {}", dest.display())),
    }
}

fn merge_dir(src: &Path, dest: &Path) -> Result<()> {
    let dm = fs::symlink_metadata(dest)?;
    if !dm.is_dir() {
        bail!("{}: not a dir", dest.display());
    }
    for child in sorted_dir(src)? {
        place_entry(&child, &dest.join(child.file_name().unwrap()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bento-cp-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn operands() {
        let b = |n: &str, p: &str| Operand::Box {
            name: n.into(),
            path: p.into(),
        };
        assert_eq!(parse_operand("x:/a"), b("x", "/a"));
        assert_eq!(parse_operand("x:/a:b"), b("x", "/a:b"));
        assert_eq!(parse_operand("./a:b"), Operand::Host("./a:b".into()));
        assert_eq!(parse_operand("/tmp/a:b"), Operand::Host("/tmp/a:b".into()));
        assert_eq!(parse_operand("a"), Operand::Host("a".into()));
        assert_eq!(parse_operand("a:b"), b("a", "b"));
    }

    #[test]
    fn docker_cp_rules() {
        let p = |dir: &str, name: Option<&str>| Placement {
            dir: dir.into(),
            name: name.map(String::from),
        };
        use Kind::*;
        // file
        assert_eq!(place("/a/f", File, "/d", Dir).unwrap(), p("/d", Some("f")));
        assert_eq!(
            place("/a/f", File, "/d/g", File).unwrap(),
            p("/d", Some("g"))
        );
        assert_eq!(
            place("/a/f", File, "/d/g", Missing).unwrap(),
            p("/d", Some("g"))
        );
        assert!(place("/a/f", File, "/d/g/", Missing).is_err());
        assert!(place("/a/f", File, "/d/g/h", NoParent).is_err());
        // dir
        assert_eq!(
            place("/a/s", Dir, "/d/new", Missing).unwrap(),
            p("/d", Some("new"))
        );
        assert_eq!(place("/a/s", Dir, "/d", Dir).unwrap(), p("/d", Some("s")));
        assert_eq!(place("/a/s/", Dir, "/d", Dir).unwrap(), p("/d", Some("s")));
        assert_eq!(place("/a/s/.", Dir, "/d", Dir).unwrap(), p("/d", None));
        assert!(place("/a/s", Dir, "/d/f", File).is_err());
        assert!(place("/a/m", Missing, "/d", Dir).is_err());
        // relative host paths
        assert_eq!(
            place("/a/f", File, "out", Missing).unwrap(),
            p(".", Some("out"))
        );
        assert_eq!(place("/f", File, "/g", Missing).unwrap(), p("/", Some("g")));
    }

    #[test]
    fn archive_roots() {
        assert_eq!(archive_root("/a/b"), ("/a".into(), "b".into()));
        assert_eq!(archive_root("/a/b/"), ("/a".into(), "b".into()));
        assert_eq!(archive_root("/a/b/."), ("/a/b".into(), ".".into()));
        assert_eq!(archive_root("/b"), ("/".into(), "b".into()));
        assert_eq!(archive_root("/"), ("/".into(), ".".into()));
    }

    /// An archive as a hostile box might send it.
    fn evil(entries: &[(&str, tar::EntryType, &str)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, kind, data) in entries {
            let mut h = tar::Header::new_old();
            h.set_entry_type(*kind);
            h.set_mode(0o644);
            match kind {
                tar::EntryType::Symlink | tar::EntryType::Link => {
                    h.set_size(0);
                    // bypass the builder's own checks: write name bytes raw
                    h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
                    h.as_old_mut().linkname[..data.len()].copy_from_slice(data.as_bytes());
                    h.set_cksum();
                    b.append(&h, std::io::empty()).unwrap();
                }
                _ => {
                    h.set_size(data.len() as u64);
                    h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
                    h.set_cksum();
                    b.append(&h, data.as_bytes()).unwrap();
                }
            }
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn round_trip() {
        let d = tmp("rt");
        let src = d.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a"), "A").unwrap();
        fs::write(src.join("sub/b"), "B").unwrap();
        fs::set_permissions(src.join("a"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("../a", src.join("sub/l")).unwrap();
        let mut buf = vec![];
        write_archive(&src, Some("tree"), &mut buf).unwrap();
        let out = d.join("out");
        fs::create_dir(&out).unwrap();
        unpack(&buf[..], "tree", &out, Some("copy")).unwrap();
        assert_eq!(fs::read_to_string(out.join("copy/a")).unwrap(), "A");
        assert_eq!(fs::read_to_string(out.join("copy/sub/b")).unwrap(), "B");
        assert_eq!(
            fs::read_link(out.join("copy/sub/l")).unwrap(),
            Path::new("../a")
        );
        assert_eq!(
            fs::metadata(out.join("copy/a"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        // into an existing dir: merged, files replaced
        fs::write(out.join("copy/a"), "old").unwrap();
        fs::write(out.join("copy/keep"), "K").unwrap();
        unpack(&buf[..], "tree", &out, Some("copy")).unwrap();
        assert_eq!(fs::read_to_string(out.join("copy/a")).unwrap(), "A");
        assert_eq!(fs::read_to_string(out.join("copy/keep")).unwrap(), "K");
        // contents into a dir
        let mut buf2 = vec![];
        write_archive(&src, None, &mut buf2).unwrap();
        let flat = d.join("flat");
        fs::create_dir(&flat).unwrap();
        unpack(&buf2[..], ".", &flat, None).unwrap();
        assert!(flat.join("sub/b").is_file());
        // no staging dirs left behind
        assert!(fs::read_dir(&out).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".bento-cp")));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn hostile_archives_never_write_outside() {
        let d = tmp("evil");
        let victim = d.join("victim");
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("secret"), "keep").unwrap();
        let out = d.join("out");
        fs::create_dir(&out).unwrap();
        let v = victim.to_str().unwrap().to_string();
        let vs = format!("{v}/secret");
        use tar::EntryType::*;
        let cases: Vec<Vec<(&str, tar::EntryType, &str)>> = vec![
            // a link, then a file through it
            vec![
                ("t", Directory, ""),
                ("t/l", Symlink, &v),
                ("t/l/secret", Regular, "pwned"),
            ],
            // a link, then a dir through it
            vec![
                ("t", Directory, ""),
                ("t/l", Symlink, &v),
                ("t/l/x", Directory, ""),
            ],
            // .. and absolute paths
            vec![
                ("t", Directory, ""),
                ("t/../../victim/secret", Regular, "pwned"),
            ],
            vec![("/tmp/pwned", Regular, "pwned")],
            // a link at the top, then writes under it
            vec![("t", Symlink, &v), ("t/secret", Regular, "pwned")],
            // a file replaced by a link to the victim, then written again
            vec![
                ("t", Directory, ""),
                ("t/f", Regular, "x"),
                ("t/f", Symlink, &vs),
            ],
            vec![
                ("t", Directory, ""),
                ("t/f", Symlink, &vs),
                ("t/f", Regular, "pwned"),
            ],
        ];
        for (i, case) in cases.iter().enumerate() {
            let ar = evil(case);
            let _ = unpack(&ar[..], "t", &out, Some(&format!("c{i}")));
            assert_eq!(
                fs::read_to_string(victim.join("secret")).unwrap(),
                "keep",
                "case {i}"
            );
            assert_eq!(fs::read_dir(&victim).unwrap().count(), 1, "case {i}");
        }
        assert!(!Path::new("/tmp/pwned").exists());
        // a hard link is never made
        let ar = evil(&[("t", Directory, ""), ("t/h", Link, &format!("{v}/secret"))]);
        unpack(&ar[..], "t", &out, Some("hl")).unwrap();
        assert!(!out.join("hl/h").exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn placing_never_follows_a_host_link_at_the_target() {
        let d = tmp("target");
        let victim = d.join("victim");
        fs::create_dir(&victim).unwrap();
        let out = d.join("out");
        fs::create_dir(&out).unwrap();
        symlink(&victim, out.join("t")).unwrap();
        let src = d.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f"), "x").unwrap();
        let mut buf = vec![];
        write_archive(&src, Some("t"), &mut buf).unwrap();
        assert!(unpack(&buf[..], "t", &out, Some("t")).is_err());
        assert_eq!(fs::read_dir(&victim).unwrap().count(), 0);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn host_kinds() {
        let d = tmp("kinds");
        fs::write(d.join("f"), "").unwrap();
        symlink("/nonexistent", d.join("l")).unwrap();
        assert_eq!(host_kind(&d), Kind::Dir);
        assert_eq!(host_kind(&d.join("f")), Kind::File);
        assert_eq!(host_kind(&d.join("l")), Kind::File);
        assert_eq!(host_kind(&d.join("m")), Kind::Missing);
        assert_eq!(host_kind(&d.join("m/x")), Kind::NoParent);
        fs::remove_dir_all(&d).unwrap();
    }
}
