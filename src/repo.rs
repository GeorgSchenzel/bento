//! The checkout a box belongs to: discovery, `bento.yaml`, box naming and
//! linked-worktree git plumbing.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context as _, Result};

/// A linked worktree: its .git is a file pointing into the main checkout's
/// .git/worktrees/<id>, which lives outside the worktree bind.
pub struct WorktreeGit {
    pub main_git: PathBuf,
    pub wt_id: String,
}

pub struct Repo {
    /// Canonical checkout path (symlinked dirs bind their real target).
    pub dir: PathBuf,
    /// Box (sandbox) name.
    pub name: String,
    pub wt: Option<WorktreeGit>,
    /// Repo-relative dirs shadowed with box-local copies (bento.yaml).
    pub overlays: Vec<String>,
    /// Guest-write quota for every writable bind mount (bento.yaml).
    pub quota_mib: u32,
}

/// Repo-root `bento.yaml`. Unknown keys are rejected so typos fail loudly.
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

/// The checkout containing the working directory: the nearest ancestor with
/// a bento.yaml or a .git.
pub fn discover() -> Result<Repo> {
    let mut dir = std::env::current_dir().context("cannot determine working directory")?;
    loop {
        if dir.join("bento.yaml").is_file() || dir.join(".git").exists() {
            return repo_at(dir);
        }
        if !dir.pop() {
            bail!("not inside a git checkout (no .git or bento.yaml in this directory or any parent)");
        }
    }
}

/// Boxes are one per checkout: `bento_{project}` for a main checkout,
/// `bento_{project}_{dir}` for a linked worktree. Collisions are caught by
/// the bento.path label guard, not avoided via hashes.
fn repo_at(dir: PathBuf) -> Result<Repo> {
    let dir = fs::canonicalize(&dir).unwrap_or(dir);
    let wt = detect_worktree(&dir);
    let project = git(&dir, &["config", "--get", "remote.origin.url"])
        .map(|url| remote_project(&url))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| match &wt {
            // no remote: name the project after the main checkout
            Some(wt) if wt.main_git.ends_with(".git") => {
                slug(&dir_basename(wt.main_git.parent().unwrap_or(&wt.main_git)))
            }
            Some(wt) => slug(&dir_basename(&wt.main_git)),
            None => slug(&dir_basename(&dir)),
        });
    let name = if wt.is_some() {
        format!("bento_{project}_{}", slug(&dir_basename(&dir)))
    } else {
        format!("bento_{project}")
    };
    let cfg = load_config(&dir)?;
    Ok(Repo {
        // msb caps sandbox names at 128 bytes
        name: truncate(&name, 128).to_string(),
        dir,
        wt,
        overlays: cfg.overlay,
        quota_mib: cfg.quota_gib.unwrap_or(DEFAULT_QUOTA_GIB) * 1024,
    })
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
            bail!("{}: quota_gib must be 1..={}", path.display(), u32::MAX / 1024);
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
        if rel.starts_with(".bento") {
            bail!("{}: overlay '{entry}' may not live under .bento", path.display());
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

/// Project slug from a remote URL: the path's last segment sans .git.
fn remote_project(url: &str) -> String {
    let base = url.rsplit(['/', ':']).next().unwrap_or(url);
    slug(base.strip_suffix(".git").unwrap_or(base))
}

/// microsandbox names allow only [A-Za-z0-9._-] and must start alphanumeric
/// (always satisfied by the "bento_" prefix).
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
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Detect a linked worktree. Its .git is a file containing a `gitdir:`
/// pointer into the main checkout's .git/worktrees/<id>. Submodules use the
/// same file format but point into modules/ — not worktrees.
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
