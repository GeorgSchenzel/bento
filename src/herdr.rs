//! hc (herdr-core) machines on the host core: every workforest box becomes
//! one when it is created (decisions §4), so the host's core lists and
//! drives the agents of every forest. Host → box only; boxes never get the
//! host core.
//!
//! Registration is opt-in through `BENTO_HC` (the host hc binary's path):
//! without it bento touches no core at all — no PATH fallback, so nothing
//! reaches the user's real core by accident. Failures only warn: a box
//! without its machine is still a box, just not in `hc ls`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};

use crate::bridge;
use crate::forest::Forest;
use crate::sandbox;

/// Is a host hc core configured (`BENTO_HC`)? The handoff (§5) answers
/// with a warning instead of a terminal when it is not.
pub fn configured() -> bool {
    hc_bin().is_some()
}

/// Start a handoff's first agent on its forest's machine (decisions §5):
/// `hc run --bg --name lead --cwd /workspace opencode <brief>` — the box's
/// `opencode` template — with `--json` to parse the terminal address
/// (`<machine>/tN`). A freshly registered machine is still connecting for
/// a few seconds (box boot), and `run --on` refuses while it is: retry
/// the way a person would re-run it.
pub fn handoff_lead(machine: &str, brief: &str) -> Result<String> {
    let hc =
        hc_bin().ok_or_else(|| anyhow!("BENTO_HC is not set: no hc core to start the agent on"))?;
    let args = lead_args(machine, brief);
    let mut last = String::new();
    for attempt in 0..LEAD_TRIES {
        if attempt > 0 {
            std::thread::sleep(LEAD_RETRY_GAP);
        }
        match run_capture(&hc, &args) {
            Ok(stdout) => return lead_address(&stdout),
            Err(e) => {
                let message = format!("{e:#}");
                if !retryable(&message) {
                    return Err(e);
                }
                last = message;
            }
        }
    }
    bail!("the machine '{machine}' never connected to the hc core: {last}")
}

/// How often and how long to retry `run --on` a connecting machine: the
/// box settles in ~5 s (H1b); 45 s covers a slow boot and stays inside the
/// client's 120 s reply timeout next to the box create.
const LEAD_TRIES: u32 = 15;
const LEAD_RETRY_GAP: Duration = Duration::from_secs(3);

/// Errors worth retrying: the machine exists but is not ready yet. hc's
/// own hints say "try again" for these.
fn retryable(message: &str) -> bool {
    message.contains("still connecting")
        || message.contains("unreachable")
        || message.contains("is not connected")
}

/// The argv of the lead run (decisions §5): flags first, so the brief
/// (which starts with the handoff header, never a `-`) stays the
/// template's task.
fn lead_args(machine: &str, brief: &str) -> Vec<String> {
    vec![
        "--json".into(),
        "run".into(),
        "--bg".into(),
        "--on".into(),
        machine.into(),
        "--name".into(),
        "lead".into(),
        "--cwd".into(),
        "/workspace".into(),
        "opencode".into(),
        brief.into(),
    ]
}

/// hc's `--json` run output: one object with `address` (`<machine>/tN`).
fn lead_address(stdout: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .with_context(|| format!("hc run printed invalid JSON: {}", stdout.trim()))?;
    v.get("address")
        .and_then(|a| a.as_str())
        .map(String::from)
        .with_context(|| format!("hc run output has no address: {}", stdout.trim()))
}

/// Run hc and capture stdout (the `--json` payload); argv only.
fn run_capture(hc: &Path, args: &[String]) -> Result<String> {
    let out = Command::new(hc)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", hc.display()))?;
    if !out.status.success() {
        bail!(
            "`hc {}` exited with {}: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Register this forest's box as a machine on the host hc core, replacing
/// any entry of the same name. Called at the end of box creation — `bento
/// start`, and the creates that `enter`/`exec`/… fall back to. Warns,
/// never fails the bento command.
pub fn register(forest: &Forest) {
    if let Err(e) = try_register(forest) {
        eprintln!("bento: registering hc machine: {e:#}");
    }
}

fn try_register(forest: &Forest) -> Result<()> {
    // only workforest boxes are machines (decisions §4)
    if bridge::workforest_name(&forest.dir).is_none() {
        return Ok(());
    }
    let Some(hc) = hc_bin() else {
        return Ok(());
    };
    let machine = machine_at(&forest.dir, &sandbox::state_dir(forest)?);
    // replace, not add: `machine add` is not idempotent (name_taken), and a
    // recreated box core has a new id anyway
    let _ = run(&hc, &rm_args(&machine));
    run(&hc, &add_args(&machine, &via(&msb_path()?, &forest.name)))?;
    eprintln!("hc machine '{machine}': registered");
    Ok(())
}

/// Remove a box's machine entry (`bento rm` and `prune`), before the state
/// dir — which holds the machine's name — goes. Silent: a machine that was
/// never added (box never started with `BENTO_HC`, or a single checkout's
/// box, which is never registered) is the normal case.
pub fn unregister(dir: &Path, box_name: &str) {
    if bridge::workforest_name(dir).is_none() {
        return;
    }
    let Some(hc) = hc_bin() else {
        return;
    };
    let machine = match sandbox::box_state_dir(box_name) {
        Ok(state) => machine_at(dir, &state),
        Err(_) => return,
    };
    if run(&hc, &rm_args(&machine)).is_ok() {
        eprintln!("hc machine '{machine}': unregistered");
    }
}

/// The host hc binary, from `BENTO_HC` only (decisions §4: no PATH
/// fallback during the spike).
fn hc_bin() -> Option<PathBuf> {
    let path = std::env::var_os("BENTO_HC")?;
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// The hc machine name this box is registered under: bridge.json's record
/// (the identity every other part reads too, decisions §4), or — when that
/// is missing — the same derivation bridge.json would use.
fn machine_at(dir: &Path, state: &Path) -> String {
    if let Ok(text) = fs::read_to_string(state.join("bridge.json")) {
        if let Ok(info) = serde_json::from_str::<bridge::BoxInfo>(&text) {
            return info.machine;
        }
    }
    bridge::machine_name(&file_name(dir))
}

fn file_name(dir: &Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The machine's via line (decisions §4, R1-proven): `msb exec --stream` is
/// byte-exact bidirectional stdio without a PTY — what hc's machine
/// protocol needs — and the line must end in `sh -c` because hc appends the
/// remote line to it as one argument. Not `bento exec`: it would start or
/// recreate stopped or removed boxes from hc's reconnect loop.
fn via(msb: &Path, box_name: &str) -> String {
    format!(
        "{} exec --stream {} -- sh -c",
        shell_word(&msb.display().to_string()),
        box_name
    )
}

/// hc runs the via as a shell script (`sh -c '<via> "$1"'`), so the msb
/// path is quoted when it is not already a plain shell word. Box names
/// (bento's slug charset) never need it.
fn shell_word(s: &str) -> String {
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The msb binary, resolved the way bento resolves it everywhere else (env
/// overrides like MSB_PATH, then the microsandbox home), never a PATH
/// lookup.
fn msb_path() -> Result<PathBuf> {
    microsandbox::config::GlobalConfig::default()
        .resolve_msb_path()
        .map_err(|e| anyhow!("resolving the msb binary: {e}"))
}

fn rm_args(machine: &str) -> Vec<String> {
    ["machine", "rm", machine].map(String::from).into()
}

fn add_args(machine: &str, via: &str) -> Vec<String> {
    vec![
        "machine".into(),
        "add".into(),
        "--name".into(),
        machine.into(),
        "--via".into(),
        via.into(),
    ]
}

/// Run hc; argv only, no shell. Fails on a nonzero exit with hc's stderr.
fn run(hc: &Path, args: &[String]) -> Result<()> {
    let out = Command::new(hc)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", hc.display()))?;
    if !out.status.success() {
        bail!(
            "`hc {}` exited with {}: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decisions §5's run, argv never a shell; the brief is one argument.
    #[test]
    fn lead_argv_is_built_as_decisions_say() {
        assert_eq!(
            lead_args("spike-f3-made", "Handoff from forest x. Task."),
            [
                "--json",
                "run",
                "--bg",
                "--on",
                "spike-f3-made",
                "--name",
                "lead",
                "--cwd",
                "/workspace",
                "opencode",
                "Handoff from forest x. Task.",
            ]
        );
    }

    /// hc's run output parsed to the address; anything else is an error.
    #[test]
    fn the_lead_address_comes_from_hc_json() {
        let out = r#"{"address":"spike-f3-made/t1","name":"lead","state":"live"}"#;
        assert_eq!(lead_address(out).unwrap(), "spike-f3-made/t1");
        assert!(lead_address("not json").is_err());
        assert!(lead_address(r#"{"address":7}"#).is_err());
        assert!(lead_address(r#"{"name":"lead"}"#).is_err());
    }

    /// Only not-yet-connected machines retry; a usage error must not.
    #[test]
    fn only_connection_errors_retry() {
        assert!(retryable("unavailable: still connecting to spike-f3-made"));
        assert!(retryable("spike-f3-made is unreachable: ..."));
        assert!(retryable("spike-f3-made is not connected"));
        assert!(!retryable("usage: --parent and --no-parent together"));
        assert!(!retryable("name_taken: lead"));
    }

    /// Decisions §4's exact line, built from parts, argv never a shell.
    #[test]
    fn machine_argv_is_built_as_decisions_say() {
        assert_eq!(
            rm_args("spike-h1b-machines"),
            ["machine", "rm", "spike-h1b-machines"]
        );
        let via = via(
            Path::new("/home/g/.microsandbox/bin/msb"),
            "bento-forest_spike-h1b-machines",
        );
        assert_eq!(
            via,
            "/home/g/.microsandbox/bin/msb exec --stream bento-forest_spike-h1b-machines -- sh -c"
        );
        assert_eq!(
            add_args("spike-h1b-machines", &via),
            [
                "machine",
                "add",
                "--name",
                "spike-h1b-machines",
                "--via",
                &via,
            ]
        );
    }

    /// The msb path crosses into shell land (hc runs the via with sh -c), so
    /// a path that is not a plain word is quoted; box names never are.
    #[test]
    fn only_odd_paths_get_quoted_in_the_via() {
        assert_eq!(
            via(
                Path::new("/nix/store/abc-msb-1.2/bin/msb"),
                "bento-forest_x"
            ),
            "/nix/store/abc-msb-1.2/bin/msb exec --stream bento-forest_x -- sh -c"
        );
        assert_eq!(
            via(Path::new("/opt/my tools/msb"), "bento-forest_x"),
            "'/opt/my tools/msb' exec --stream bento-forest_x -- sh -c"
        );
        // a single quote itself survives hc's sh -c line
        assert_eq!(shell_word("/opt/it's/msb"), r"'/opt/it'\''s/msb'");
    }
}
