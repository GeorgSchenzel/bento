//! Coding agents in a box. Each box keeps its own agent state in a plain host
//! dir (sqlite/locks don't work across VMs, so state is never shared between
//! boxes); the host's agent config is layered on top read-only, so the box
//! sees the same plugins/skills/settings but can't modify them.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::sandbox::Mount;

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

/// Long-lived token from `claude setup-token`: the host's claude credentials
/// live in the macOS Keychain, which can't be mounted.
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

    /// Per-run env (credentials stay out of the sandbox config).
    pub fn env(&self, home: &Path) -> Result<Vec<(String, String)>> {
        match self {
            Agent::Claude => {
                let path = home.join(CLAUDE_TOKEN);
                let token = fs::read_to_string(&path).with_context(|| {
                    format!(
                        "reading the claude token {}; create it with:\n  claude setup-token  # then save the printed token to that file",
                        path.display()
                    )
                })?;
                Ok(vec![(
                    "CLAUDE_CODE_OAUTH_TOKEN".into(),
                    token.trim().to_string(),
                )])
            }
            Agent::Opencode => Ok(vec![]),
        }
    }
}

/// Sandbox-wide env for the agents.
pub fn sandbox_env(home: &Path) -> Vec<(String, String)> {
    // puts .claude.json inside the per-box state dir instead of $HOME
    vec![(
        "CLAUDE_CONFIG_DIR".into(),
        home.join(".claude").display().to_string(),
    )]
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
