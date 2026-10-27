//! What a box belongs to: a forest of checkouts (trees). A forest is a
//! `forest` workforest (`<root>/forests/<name>/<project>…`, beside
//! `<root>/repos/`); a single checkout is a forest with one tree. Discovery, `bento.yaml`, box naming
//! and linked-worktree git plumbing.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context as _, Result};

/// Guest mountpoint of the forest; each tree is bound at `/workspace/<tree>`.
pub const WORKDIR: &str = "/workspace";

/// `forest`'s dirs under its root: workforests, and the projects' clones.
const FORESTS: &str = "forests";
const REPOS: &str = "repos";

/// A linked worktree: its .git is a file pointing into the main checkout's
/// git dir at worktrees/<id>, which lives outside the worktree bind.
pub struct WorktreeGit {
    pub main_git: PathBuf,
    pub wt_id: String,
}

pub struct Tree {
    /// Dir under /workspace: the project's name in the host layout.
    pub name: String,
    /// Canonical checkout path (symlinked dirs bind their real target).
    pub dir: PathBuf,
    pub wt: Option<WorktreeGit>,
    /// Tree-relative dirs shadowed with box-local copies (bento.yaml).
    pub overlays: Vec<String>,
    /// Guest-write quota for the tree's writable bind mounts (bento.yaml).
    pub quota_mib: u32,
}

pub struct Forest {
    /// Box (sandbox) name.
    pub name: String,
    /// Host dir the box belongs to: the workforest, or the single checkout.
    pub dir: PathBuf,
    pub trees: Vec<Tree>,
    /// Guest working dir matching the host's: `/workspace/<tree>/<rel>`.
    pub cwd: String,
}

impl Forest {
    /// Quota for the forest-wide binds (the /workspace root).
    pub fn quota_mib(&self) -> u32 {
        self.trees
            .iter()
            .map(|t| t.quota_mib)
            .max()
            .unwrap_or(DEFAULT_QUOTA_GIB * 1024)
    }
}

/// Tree-root `bento.yaml`. Unknown keys are rejected so typos fail loudly.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    overlay: Vec<String>,
    /// GiB the box may add to each writable bind mount. msb applies 4 GiB
    /// when unset and fails with ENOSPC past it (while guest `df` shows
    /// room), so bento always sets one; it guards the host disk, not a budget.
    quota_gib: Option<u32>,
}

const DEFAULT_QUOTA_GIB: u32 = 64;

/// The forest containing the working directory: an enclosing workforest,
/// else the nearest ancestor with a bento.yaml or a .git as a single tree.
pub fn discover() -> Result<Forest> {
    let cwd = std::env::current_dir().context("cannot determine working directory")?;
    let cwd = fs::canonicalize(&cwd).unwrap_or(cwd);
    if let Some(root) = cwd.ancestors().find(|a| is_workforest(a)) {
        return workforest(root, &cwd);
    }
    let mut dir = cwd.clone();
    loop {
        if dir.join("bento.yaml").is_file() || dir.join(".git").exists() {
            break;
        }
        if !dir.pop() {
            bail!(
                "not inside a git checkout (no .git or bento.yaml in this directory or any parent)"
            );
        }
    }
    let tree = tree_at(dir.clone(), None)?;
    // `bento_{project}` for a main checkout, `bento_{project}_{dir}` for a
    // linked worktree; collisions are caught by the bento.path label guard
    let name = match &tree.wt {
        Some(_) => format!("bento_{}_{}", tree.name, slug(&dir_basename(&dir))),
        None => format!("bento_{}", tree.name),
    };
    let rel = cwd.strip_prefix(&dir).unwrap_or(Path::new(""));
    let cwd = guest_path(&tree.name, rel);
    Ok(Forest {
        name: box_name(&name),
        dir,
        trees: vec![tree],
        cwd,
    })
}

/// `<root>/forests/<name>`; the `repos/` sibling tells forest's layout
/// apart from any dir that happens to be called "forests".
fn is_workforest(dir: &Path) -> bool {
    let Some(forests) = dir.parent() else {
        return false;
    };
    forests.file_name().is_some_and(|n| n == FORESTS)
        && forests
            .parent()
            .is_some_and(|root| root.join(REPOS).is_dir())
}

/// A workforest: its trees are the checkouts directly in it. Forest and
/// project names may be equal, so forest boxes have their own prefix.
fn workforest(root: &Path, cwd: &Path) -> Result<Forest> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)
        .with_context(|| format!("reading {}", root.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join(".git").exists())
        .collect();
    dirs.sort();
    let trees = dirs
        .into_iter()
        .map(|d| {
            let name = slug(&dir_basename(&d));
            tree_at(d, Some(name))
        })
        .collect::<Result<Vec<_>>>()?;
    if trees.is_empty() {
        bail!("forest {} has no checkouts", root.display());
    }
    // cwd in a tree maps into it; anywhere else in the forest is its root,
    // or the tree itself when there is only one
    let rel = cwd.strip_prefix(root).unwrap_or(Path::new(""));
    let cwd = match rel.components().next() {
        Some(Component::Normal(first)) => trees
            .iter()
            .find(|t| t.dir.file_name() == Some(first))
            .map(|t| guest_path(&t.name, rel.strip_prefix(first).unwrap_or(Path::new("")))),
        _ => None,
    }
    .or_else(|| match trees.as_slice() {
        [only] => Some(guest_path(&only.name, Path::new(""))),
        _ => None,
    });
    Ok(Forest {
        name: box_name(&format!("bento-forest_{}", slug(&dir_basename(root)))),
        dir: root.to_path_buf(),
        trees,
        cwd: cwd.unwrap_or_else(|| WORKDIR.into()),
    })
}

fn guest_path(tree: &str, rel: &Path) -> String {
    let mut path = format!("{WORKDIR}/{tree}");
    for c in rel.components() {
        if let Component::Normal(s) = c {
            path.push('/');
            path.push_str(&s.to_string_lossy());
        }
    }
    path
}

/// msb caps sandbox names at 128 bytes.
fn box_name(name: &str) -> String {
    truncate(name, 128).to_string()
}

fn tree_at(dir: PathBuf, name: Option<String>) -> Result<Tree> {
    let dir = fs::canonicalize(&dir).unwrap_or(dir);
    let wt = detect_worktree(&dir);
    let name = name.unwrap_or_else(|| project_name(&dir, wt.as_ref()));
    let mut cfg = load_config(&dir)?;
    // devenv's state holds platform-specific profile symlinks
    if dir.join("devenv.nix").is_file() && !cfg.overlay.iter().any(|o| o == ".devenv") {
        cfg.overlay.push(".devenv".into());
    }
    Ok(Tree {
        name,
        dir,
        wt,
        overlays: cfg.overlay,
        quota_mib: cfg.quota_gib.unwrap_or(DEFAULT_QUOTA_GIB) * 1024,
    })
}

/// The project's name in the host layout: the dir holding its git dir
/// (`<project>/.bare`, `<project>/.git`, or a bare `<project>.git`).
fn project_name(dir: &Path, wt: Option<&WorktreeGit>) -> String {
    let Some(wt) = wt else {
        return slug(&dir_basename(dir));
    };
    let git = &wt.main_git;
    let base = dir_basename(git);
    if base == ".bare" || base == ".git" {
        slug(&dir_basename(git.parent().unwrap_or(git)))
    } else {
        slug(base.strip_suffix(".git").unwrap_or(&base))
    }
}

fn load_config(dir: &Path) -> Result<Config> {
    let path = dir.join("bento.yaml");
    if !path.is_file() {
        return Ok(Config::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    // an all-comments file parses as null
    let cfg: Option<Config> =
        serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let cfg = cfg.unwrap_or_default();
    validate_overlays(&cfg.overlay, &path)?;
    if let Some(gib) = cfg.quota_gib {
        // msb takes MiB as u32
        if gib == 0 || gib > u32::MAX / 1024 {
            bail!(
                "{}: quota_gib must be 1..={}",
                path.display(),
                u32::MAX / 1024
            );
        }
    }
    Ok(cfg)
}

fn validate_overlays(entries: &[String], path: &Path) -> Result<()> {
    for entry in entries {
        if entry.trim() != *entry || entry.is_empty() || entry.chars().any(char::is_whitespace) {
            bail!("{}: bad overlay entry '{entry}'", path.display());
        }
        let rel = Path::new(entry);
        if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            bail!(
                "{}: overlay '{entry}' must be a plain relative path (no /, .. or .)",
                path.display()
            );
        }
    }
    let mut sorted: Vec<&str> = entries.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    for w in sorted.windows(2) {
        if w[0] == w[1] {
            bail!("{}: duplicate overlay '{}'", path.display(), w[0]);
        }
        if w[1].starts_with(&format!("{}/", w[0])) {
            bail!("{}: overlay '{}' overlaps '{}'", path.display(), w[1], w[0]);
        }
    }
    Ok(())
}

fn dir_basename(dir: &Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
}

/// microsandbox names allow only [A-Za-z0-9._-] and must start alphanumeric
/// (always satisfied by the "bento_" prefix); tree dirs use the same set.
fn slug(input: &str) -> String {
    let s: String = input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    if s.is_empty() {
        "repo".into()
    } else {
        s.to_string()
    }
}

fn truncate(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Detect a linked worktree. Its .git is a file containing a `gitdir:`
/// pointer into the main checkout's git dir at worktrees/<id>. Submodules
/// use the same file format but point into modules/ — not worktrees.
fn detect_worktree(dir: &Path) -> Option<WorktreeGit> {
    let git_file = dir.join(".git");
    if !git_file.is_file() {
        return None;
    }
    let text = fs::read_to_string(git_file).ok()?;
    let line = text.lines().find(|l| l.starts_with("gitdir:"))?;
    let wt_git = fs::canonicalize(dir.join(line["gitdir:".len()..].trim())).ok()?;
    if wt_git.parent()?.file_name()? != "worktrees" {
        return None;
    }
    let wt_id = wt_git.file_name()?.to_string_lossy().into_owned();
    let main_git = fs::canonicalize(wt_git.join("../..")).ok()?;
    Some(WorktreeGit { main_git, wt_id })
}
