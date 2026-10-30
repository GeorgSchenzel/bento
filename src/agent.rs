//! Coding agents in a box. Each box keeps its own agent state in a plain host
//! dir (sqlite/locks don't work across VMs, so state is never shared between
//! boxes); the host's agent config is layered on top read-only, so the box
//! sees the same plugins/skills/settings but can't modify them.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use microsandbox::sandbox::Sandbox;
use serde_json::{json, Value};

use crate::sandbox::Mount;
use crate::util;

/// Host `~/.claude` entries the box gets read-only; everything else in there
/// (history, projects, sessions, caches) is per-box state.
const CLAUDE_CONFIG: &[&str] = &[
    "settings.json",
    "CLAUDE.md",
    "skills",
    "plugins",
    "agents",
    "commands",
];

/// Long-lived token from `claude setup-token`: the host's claude login stays
/// out of the box (the macOS Keychain can't be mounted; on Linux it's
/// `~/.claude/.credentials.json`, not among CLAUDE_CONFIG). The host env var
/// wins over the file, so the shell decides which account a run uses.
const CLAUDE_TOKEN_VAR: &str = "BENTO_CLAUDE_TOKEN";
const CLAUDE_TOKEN: &str = ".config/bento/claude-token";

pub enum Agent {
    Claude,
    Opencode,
}

impl Agent {
    pub fn command(&self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Opencode => "opencode",
        }
    }

    /// Launch args: the user's args with a permission-skipping flag in
    /// front, unless they set permissions themselves. The box is the
    /// sandbox, so agents run permission-free.
    pub fn launch_args(&self, args: &[String]) -> Vec<String> {
        const PERMISSIONS: &[&str] = &[
            "--dangerously-skip-permissions",
            "--permission-mode",
            "--auto",
            "--yolo",
        ];
        if args.iter().any(|a| {
            PERMISSIONS
                .iter()
                .any(|p| a == p || a.starts_with(&format!("{p}=")))
        }) {
            return args.to_vec();
        }
        // Without the setting claude asks to accept bypass mode on every new
        // box; the host's ~/.claude/settings.json is read-only in here.
        let flags: &[&str] = match self {
            Agent::Claude => &[
                "--dangerously-skip-permissions",
                "--settings",
                r#"{"skipDangerousModePermissionPrompt":true}"#,
            ],
            Agent::Opencode => &["--auto"],
        };
        flags
            .iter()
            .map(|f| f.to_string())
            .chain(args.iter().cloned())
            .collect()
    }

    /// Per-run env (credentials stay out of the sandbox config).
    pub fn env(&self, home: &Path) -> Result<Vec<(String, String)>> {
        match self {
            Agent::Claude => {
                let token = match std::env::var(CLAUDE_TOKEN_VAR) {
                    Ok(token) if !token.trim().is_empty() => token,
                    _ => {
                        let path = home.join(CLAUDE_TOKEN);
                        fs::read_to_string(&path).with_context(|| {
                            format!(
                                "no claude token: set {CLAUDE_TOKEN_VAR}, or save one to {}; create it with:\n  claude setup-token",
                                path.display()
                            )
                        })?
                    }
                };
                Ok(vec![
                    ("CLAUDE_CODE_OAUTH_TOKEN".into(), token.trim().to_string()),
                    // The fullscreen TUI is gated by an account feature flag,
                    // which a setup-token can't fetch (no profile scope).
                    ("CLAUDE_CODE_NO_FLICKER".into(), "1".into()),
                ])
            }
            Agent::Opencode => Ok(vec![]),
        }
    }
}

/// Sandbox-wide env for the agents.
pub fn sandbox_env(home: &Path) -> Vec<(String, String)> {
    // puts .claude.json inside the per-box state dir instead of $HOME
    vec![
        (
            "CLAUDE_CONFIG_DIR".into(),
            home.join(".claude").display().to_string(),
        ),
        // claude refuses --dangerously-skip-permissions as root (the box's
        // user) unless it knows it's in a sandbox already
        ("IS_SANDBOX".into(), "1".into()),
        // neither that nor the flag covers the "trust this folder" prompt,
        // which claude asks per checkout, linked worktrees included
        ("CLAUDE_CODE_SANDBOXED".into(), "1".into()),
    ]
}

/// Agent state and config mounts. `state` is the box's host state dir.
pub fn mounts(home: &Path, state: &Path, quota: u32) -> Result<Vec<Mount>> {
    let mut mounts = Vec::new();

    let claude = state.join("claude");
    fs::create_dir_all(&claude).with_context(|| format!("creating {}", claude.display()))?;
    let claude_json = claude.join(".claude.json");
    if !claude_json.exists() {
        // skip first-run onboarding; auth comes from the token
        fs::write(&claude_json, "{\"hasCompletedOnboarding\": true}\n")
            .with_context(|| format!("writing {}", claude_json.display()))?;
    }
    mounts.push(Mount::rw(home.join(".claude"), claude, quota));
    for name in CLAUDE_CONFIG {
        if let Some(host) = canonical(&home.join(".claude").join(name)) {
            mounts.push(Mount::ro(home.join(".claude").join(name), host));
        }
    }

    let opencode = state.join("opencode");
    fs::create_dir_all(&opencode).with_context(|| format!("creating {}", opencode.display()))?;
    let share = home.join(".local/share/opencode");
    mounts.push(Mount::rw(share.clone(), opencode, quota));
    if let Some(auth) = canonical(&share.join("auth.json")) {
        mounts.push(Mount::ro(share.join("auth.json"), auth));
    }
    // Entry by entry, each resolved: some are symlinks to host-only paths
    // (home-manager's opencode.json points into the host's /nix/store).
    let config = home.join(".config/opencode");
    if let Ok(entries) = fs::read_dir(&config) {
        let mut names: Vec<_> = entries.flatten().map(|e| e.file_name()).collect();
        names.sort();
        for name in names {
            if let Some(host) = canonical(&config.join(&name)) {
                mounts.push(Mount::ro(config.join(&name), host));
            }
        }
    }
    Ok(mounts)
}

fn canonical(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

/// Files in the state dir that outlive the box, besides claude's session
/// transcripts. The db's WAL can hold its latest messages.
const ARCHIVED: &[&str] = &[
    "claude/history.jsonl",
    "opencode/opencode.db",
    "opencode/opencode.db-wal",
];

/// Move the chats out of a stopped box's state dir into `dest`, created only
/// if there are any. The guest wrote that dir: symlinks in it are never
/// followed or kept, or a box could have host files moved here. Whatever a
/// failed run already moved is simply missing on retry.
pub fn archive(state: &Path, dest: &Path) -> Result<()> {
    let mut chats: Vec<PathBuf> = ARCHIVED.iter().map(PathBuf::from).collect();
    chats.extend(claude_transcripts(state)?);
    for chat in chats {
        let from = state.join(&chat);
        if !fs::symlink_metadata(&from).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let to = dest.join(&chat);
        let parent = to.parent().context("archived file without a parent")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700) // transcripts and the db hold secrets
            .create(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        fs::rename(&from, &to)
            .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
    }
    Ok(())
}

/// `claude/projects/<project>/<session>.jsonl`, relative to the state dir;
/// not the tool-results, subagents and memory dirs beside them.
fn claude_transcripts(state: &Path) -> Result<Vec<PathBuf>> {
    let projects = Path::new("claude/projects");
    let mut transcripts = Vec::new();
    for project in real_entries(&state.join(projects), fs::Metadata::is_dir)? {
        let project = projects.join(project);
        for file in real_entries(&state.join(&project), fs::Metadata::is_file)? {
            if Path::new(&file).extension().is_some_and(|e| e == "jsonl") {
                transcripts.push(project.join(file));
            }
        }
    }
    Ok(transcripts)
}

/// Names of a dir's entries that pass `keep`, without following symlinks; a
/// dir that's missing or a symlink has none.
fn real_entries(dir: &Path, keep: fn(&fs::Metadata) -> bool) -> Result<Vec<OsString>> {
    if !fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        if fs::symlink_metadata(entry.path()).is_ok_and(|m| keep(&m)) {
            names.push(entry.file_name());
        }
    }
    Ok(names)
}

/// opencode v2 keeps credentials in its sqlite db. It imports the v1
/// auth.json only when migrating a v1 db, never into a fresh one, so a box
/// would start without any. Hand the host's keys over with `opencode auth
/// import`, again whenever they change. Stable ids make re-imports skip
/// credentials the box already has.
pub async fn import_opencode_credentials(sb: &Sandbox, home: &Path, state: &Path) -> Result<()> {
    let Some(json) = opencode_credentials(home)? else {
        return Ok(());
    };
    let marker = state.join("opencode/.bento-auth");
    let fp = util::fingerprint(&[&json]);
    if fs::read_to_string(&marker).ok().as_deref() == Some(fp.as_str()) {
        return Ok(());
    }
    const FILE: &str = "/tmp/bento-opencode-auth.json";
    sb.fs().write(FILE, json).await?;
    let out = sb
        .shell(format!(
            "opencode auth import --standalone {FILE}; rc=$?; rm -f {FILE}; exit $rc"
        ))
        .await?;
    if !out.status().success {
        bail!("{}", out.stderr().unwrap_or_default().trim());
    }
    fs::write(&marker, fp).with_context(|| format!("writing {}", marker.display()))?;
    Ok(())
}

/// The host's v1 auth.json in `opencode auth export` format. Only API keys:
/// oauth entries need v2's per-integration method ids.
fn opencode_credentials(home: &Path) -> Result<Option<String>> {
    let path = home.join(".local/share/opencode/auth.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(None);
    };
    let auth: serde_json::Map<String, Value> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let mut entries = Vec::new();
    for (id, v) in &auth {
        let mut value = match v["type"].as_str() {
            Some("api") => json!({ "type": "key", "key": v["key"] }),
            Some("wellknown") => json!({ "type": "key", "key": v["token"] }),
            _ => continue,
        };
        if let Some(meta) = v.get("metadata") {
            value["metadata"] = meta.clone();
        }
        let id = id.trim_end_matches('/');
        entries.push(json!({
            "id": format!("cred_bento_{}", util::fingerprint(&[id])),
            "integrationID": id,
            "label": "auth.json",
            "active": true,
            "value": value,
        }));
    }
    Ok((!entries.is_empty()).then(|| Value::Array(entries).to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt as _};

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bento-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn archives_only_the_chats() {
        let root = tmp("whitelist");
        let (state, dest) = (root.join("state"), root.join("dest"));
        for kept in [
            "claude/history.jsonl",
            "claude/projects/p/s.jsonl",
            "opencode/opencode.db",
            "opencode/opencode.db-wal",
        ] {
            write(&state.join(kept));
        }
        for dropped in [
            "claude/.claude.json",
            "claude/file-history/s/f",
            "claude/projects/p/s/tool-results/t.jsonl",
            "claude/projects/p/s/subagents/a.jsonl",
            "claude/projects/p/memory/m.md",
            "opencode/opencode.db-shm",
            "opencode/snapshot/h/f",
            "opencode/log/l",
        ] {
            write(&state.join(dropped));
        }
        archive(&state, &dest).unwrap();
        assert!(dest.join("claude/history.jsonl").is_file());
        assert!(dest.join("claude/projects/p/s.jsonl").is_file());
        assert!(dest.join("opencode/opencode.db").is_file());
        assert!(dest.join("opencode/opencode.db-wal").is_file());
        assert!(!state.join("opencode/opencode.db").exists());
        assert!(!dest.join("claude/projects/p/s").exists());
        assert!(!dest.join("claude/projects/p/memory").exists());
        assert!(!dest.join("opencode/snapshot").exists());
        assert!(!dest.join("opencode/opencode.db-shm").exists());
        assert!(!dest.join("claude/.claude.json").exists());
        assert_eq!(
            fs::metadata(dest.join("claude"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn never_touches_what_guest_symlinks_point_at() {
        let root = tmp("symlinks");
        let (state, dest, host) = (root.join("state"), root.join("dest"), root.join("host"));
        write(&host.join("p/secret.jsonl"));
        write(&host.join("secret.jsonl"));
        fs::create_dir_all(state.join("claude")).unwrap();
        fs::create_dir_all(state.join("opencode")).unwrap();
        // the projects dir, a project and a transcript, and a fixed path
        symlink(&host, state.join("claude/projects")).unwrap();
        symlink(
            host.join("secret.jsonl"),
            state.join("opencode/opencode.db"),
        )
        .unwrap();
        archive(&state, &dest).unwrap();
        assert!(host.join("p/secret.jsonl").is_file());
        assert!(host.join("secret.jsonl").is_file());
        assert!(!dest.exists());

        fs::remove_file(state.join("claude/projects")).unwrap();
        fs::create_dir_all(state.join("claude/projects/real")).unwrap();
        symlink(host.join("p"), state.join("claude/projects/link")).unwrap();
        symlink(
            host.join("secret.jsonl"),
            state.join("claude/projects/real/s.jsonl"),
        )
        .unwrap();
        archive(&state, &dest).unwrap();
        assert!(host.join("p/secret.jsonl").is_file());
        assert!(host.join("secret.jsonl").is_file());
        assert!(!dest.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn nothing_to_archive_creates_nothing() {
        let root = tmp("empty");
        let dest = root.join("dest");
        archive(&root.join("missing"), &dest).unwrap();
        assert!(!dest.exists());
        fs::remove_dir_all(&root).unwrap();
    }
}
