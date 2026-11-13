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
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::util;

/// Default daemon port, next to the builder's 17760.
pub const DEFAULT_PORT: u16 = 17761;

/// How boxes address the host: msb rewrites the gateway to the host's
/// loopback (R2: the only path boxes are guaranteed to reach; the LAN IP
/// is not reachable from a box).
pub const HOST: &str = "host.microsandbox.internal";

/// An idle connection is closed after this long.
const IDLE: Duration = Duration::from_secs(10 * 60);

/// One request line may be at most this long.
const MAX_LINE: usize = 1024 * 1024;

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

/// hc's id alphabet (terminals.rs `SYMBOLS`): digits plus the consonants it
/// draws terminal ids from. A name `t` + these is a terminal id shape.
const HC_ID_ALPHABET: &[u8; 30] = b"0123456789bcdfghjkmnpqrstvwxyz";

/// hc's rule (terminals.rs `id_shaped`): a name that could be a terminal id
/// (`t`, then 1-9 and only id-alphabet chars — `t440s` and `t1b` count) must
/// not be used as a machine name.
fn id_shaped(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('t') else {
        return false;
    };
    let mut bytes = rest.bytes();
    matches!(bytes.next(), Some(b'1'..=b'9')) && bytes.all(|b| HC_ID_ALPHABET.contains(&b))
}

// ---------------------------------------------------------------------------
// The daemon (`bento serve`): TCP on 127.0.0.1, JSON lines — one reply per
// request, in order; the first request on a connection must be `hello`.
// One thread per connection, so a slow forest command never blocks the
// others. Protocol and ops: decisions.md §1.
// ---------------------------------------------------------------------------

/// An error as it crosses the wire: `{"ok":false,"error":{code,message}}`.
#[derive(Debug)]
struct BridgeError {
    code: &'static str,
    message: String,
}

impl BridgeError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        BridgeError {
            code,
            message: message.into(),
        }
    }
}

/// The authenticated caller of a connection: everything the daemon knows
/// about the far end, derived from the token alone.
pub struct Caller {
    pub box_name: String,
    /// The forest or checkout dir this box's ops act on.
    pub path: PathBuf,
    /// The workforest's dir name; None for a single checkout's box.
    pub forest: Option<String>,
    /// The hc machine name (decisions §4), from bridge.json.
    #[allow(dead_code)] // first read by F3 (the handoff) and ois
    pub machine: String,
    /// The daemon's runtime handle: a handler that must call bento's async
    /// code (F3's handoff creates a box) uses it from its own thread.
    #[allow(dead_code)] // first read by F3/G1; no op needs it yet
    pub rt: tokio::runtime::Handle,
}

struct Daemon {
    data_dir: PathBuf,
    rt: tokio::runtime::Handle,
}

/// Run the bridge daemon until killed. Started by the user (a systemd
/// --user unit later); nothing auto-starts it.
pub async fn serve(port: u16) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .with_context(|| format!("binding 127.0.0.1:{port}"))?;
    let daemon = Arc::new(Daemon {
        data_dir: util::data_dir()?,
        rt: tokio::runtime::Handle::current(),
    });
    eprintln!("bento serve: listening on 127.0.0.1:{port} (boxes: {HOST}:{port})");
    // blocking accept on a dedicated thread, off the async runtime
    tokio::task::spawn_blocking(move || loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let daemon = daemon.clone();
                let _ = std::thread::Builder::new()
                    .name("bridge-conn".into())
                    .spawn(move || handle_conn(&daemon, stream));
            }
            Err(e) => {
                eprintln!("bento serve: accept: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    })
    .await
    .map_err(|e| anyhow!("the accept loop died: {e}"))?;
    Ok(())
}

fn handle_conn(daemon: &Daemon, stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(IDLE));
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    let mut caller: Option<Caller> = None;
    loop {
        let line = match read_line(&mut reader, MAX_LINE) {
            Ok(Line::Complete(bytes)) => bytes,
            Ok(Line::Eof) => break,
            Ok(Line::TooLong) => {
                let err = BridgeError::new(
                    "bad_request",
                    format!("a request line may be at most {MAX_LINE} bytes"),
                );
                let _ = write_reply(&mut writer, &err_reply(&err));
                break;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                eprintln!("bento serve: closed an idle connection");
                break;
            }
            Err(_) => break,
        };
        // blank lines are skipped: cheap keepalives for hand-testing
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let started = Instant::now();
        let (op, reply) = match parse_request(&line) {
            Ok((op, req)) => {
                let reply = match handle_request(daemon, &mut caller, &op, &req) {
                    Ok(result) => ok_reply(result),
                    Err(e) => err_reply(&e),
                };
                (op, reply)
            }
            Err(e) => ("-".into(), err_reply(&e)),
        };
        eprintln!(
            "bento serve: {} {} {} ({} ms)",
            caller.as_ref().map(|c| c.box_name.as_str()).unwrap_or("-"),
            op,
            if reply["ok"].as_bool().unwrap_or(false) {
                "ok".to_string()
            } else {
                format!("err: {}", reply["error"]["code"].as_str().unwrap_or("?"))
            },
            started.elapsed().as_millis()
        );
        if write_reply(&mut writer, &reply).is_err() {
            break;
        }
    }
}

/// A request line: a JSON object with a string `op`.
fn parse_request(line: &[u8]) -> Result<(String, Value), BridgeError> {
    let req: Value = serde_json::from_slice(line)
        .map_err(|e| BridgeError::new("bad_request", format!("invalid JSON: {e}")))?;
    if !req.is_object() {
        return Err(BridgeError::new(
            "bad_request",
            "a request must be a JSON object",
        ));
    }
    let op = req
        .get("op")
        .and_then(Value::as_str)
        .filter(|op| !op.is_empty())
        .ok_or_else(|| BridgeError::new("bad_request", "missing string field 'op'"))?
        .to_string();
    Ok((op, req))
}

fn handle_request(
    daemon: &Daemon,
    caller: &mut Option<Caller>,
    op: &str,
    req: &Value,
) -> Result<Value, BridgeError> {
    if op == "hello" {
        let c = hello(daemon, req)?;
        let result = json!({
            "box": c.box_name,
            "path": c.path.display().to_string(),
            "forest": c.forest,
            "workforest": c.forest.is_some(),
        });
        *caller = Some(c);
        return Ok(result);
    }
    let c = caller.as_ref().ok_or_else(|| {
        BridgeError::new(
            "hello_required",
            "the first request on a connection must be hello",
        )
    })?;
    dispatch(c, op, req)
}

/// The dispatch table: one arm per op, so G1 (gpu.*) and F3 (forest.new's
/// brief) add theirs without touching the plumbing above.
fn dispatch(caller: &Caller, op: &str, req: &Value) -> Result<Value, BridgeError> {
    match op {
        "forest.ls" => forest_ls(caller),
        "forest.add" => forest_add(caller, req),
        "forest.name" => forest_name(caller, req),
        "forest.new" => forest_new(caller, req),
        op if op.starts_with("gpu.") => Err(BridgeError::new(
            "unknown_op",
            format!("'{op}' arrives with G1 (the GPU queue)"),
        )),
        _ => Err(BridgeError::new("unknown_op", format!("unknown op '{op}'"))),
    }
}

/// Authenticate the connection by token. The boxes' bridge.json files are
/// re-read on every hello: cheap (one per forest), and never stale when
/// boxes come and go. Unreadable or invalid files are skipped.
fn hello(daemon: &Daemon, req: &Value) -> Result<Caller, BridgeError> {
    let token = req
        .get("token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| BridgeError::new("bad_request", "missing string field 'token'"))?;
    let info = lookup_token(&daemon.data_dir, token).ok_or_else(|| {
        BridgeError::new(
            "unknown_token",
            "no box has this token; was the box recreated?",
        )
    })?;
    let path = PathBuf::from(&info.path);
    Ok(Caller {
        forest: workforest_name(&path),
        path,
        machine: info.machine,
        box_name: info.box_name,
        rt: daemon.rt.clone(),
    })
}

fn lookup_token(data_dir: &Path, token: &str) -> Option<BoxInfo> {
    let mut infos = Vec::new();
    for entry in fs::read_dir(data_dir.join("boxes")).ok()?.flatten() {
        let Ok(text) = fs::read_to_string(entry.path().join("bridge.json")) else {
            continue; // a box being removed, or mid-write
        };
        match serde_json::from_str::<BoxInfo>(&text) {
            Ok(info) => infos.push(info),
            Err(_) => continue, // corrupt: never fatal
        }
    }
    infos.into_iter().find(|i| i.token == token)
}

// --- the forest ops -------------------------------------------------------

fn forest_ls(caller: &Caller) -> Result<Value, BridgeError> {
    let envelope = run_forest(&caller.path, &ls_args())?;
    let repos = ls_repos(&envelope)?;
    let forest = if caller.forest.is_some() {
        find_forest(&envelope, &caller.path)
    } else {
        Value::Null
    };
    Ok(json!({ "repos": repos, "forest": forest }))
}

/// `forest.add`: add repos to this forest. Needs `bento start` afterwards,
/// which recreates the box and stops its agents — the reply says so.
fn forest_add(caller: &Caller, req: &Value) -> Result<Value, BridgeError> {
    require_workforest(caller)?;
    let repos = string_list(req, "repos")?;
    if repos.is_empty() {
        return Err(BridgeError::new(
            "bad_request",
            "'repos' must be a non-empty list of repo names",
        ));
    }
    validate_repos(caller, &repos)?;
    run_forest(&caller.path, &add_args(&repos))?;
    Ok(json!({
        "added": repos,
        "restart_needed": true,
        "hint": "repos are mounted by `bento start`, which recreates the box and stops its agents; add repos before launching agents"
    }))
}

/// `forest.name`: rename this forest's branch. The forest dir is not
/// renamed (that would change the box and kill its agents) — the hint
/// nudges `forest mv` + `bento start` for later.
fn forest_name(caller: &Caller, req: &Value) -> Result<Value, BridgeError> {
    require_workforest(caller)?;
    let branch = string_field(req, "branch")?;
    valid_branch(&branch)?;
    let old = match find_forest(&run_forest(&caller.path, &ls_args())?, &caller.path) {
        Value::Null => Value::Null,
        f => f["branch"].clone(),
    };
    run_forest(&caller.path, &name_args(&branch))?;
    Ok(json!({
        "old": old,
        "new": branch,
        "hint": "the forest folder keeps its name (renaming it would change the box and kill its agents); rename it with `forest mv` + `bento start` later, when no agents run"
    }))
}

/// `forest.new`: create a new forest on the host. Without a brief that is
/// all; with one it becomes a handoff (new box, first agent), which F3
/// adds. Always `--no-fetch --create-only`: fetching is the user's, and
/// never join an existing forest (e.g. the user's).
fn forest_new(caller: &Caller, req: &Value) -> Result<Value, BridgeError> {
    if req.get("brief").map(|b| !b.is_null()).unwrap_or(false) {
        return Err(BridgeError::new(
            "failed",
            "handoff not implemented yet (F3 adds the brief: a new box and a first agent)",
        ));
    }
    let name = string_field(req, "name")?;
    valid_forest_name(&name)?;
    let repos = string_list(req, "repos")?;
    if repos.is_empty() {
        return Err(BridgeError::new(
            "bad_request",
            "'repos' must be a non-empty list of repo names",
        ));
    }
    let from = opt_string_field(req, "from")?;
    if let Some(from) = &from {
        valid_branch(from)?;
    }
    validate_repos(caller, &repos)?;
    // any cwd works (forest resolves its root); the forest root's parent
    // keeps `forest new` away from this box's own forest
    let cwd = caller.path.parent().unwrap_or(&caller.path).to_path_buf();
    let envelope = run_forest(&cwd, &new_args(&name, &repos, from.as_deref()))?;
    Ok(json!({
        "name": envelope.get("forest").cloned().unwrap_or(Value::Null),
        "path": envelope.get("path").cloned().unwrap_or(Value::Null),
    }))
}

/// These ops act on "this forest", which a single checkout's box has not.
fn require_workforest(caller: &Caller) -> Result<(), BridgeError> {
    if caller.forest.is_some() {
        Ok(())
    } else {
        Err(BridgeError::new(
            "not_a_forest",
            "this op needs the box of a forest, not of a single checkout",
        ))
    }
}

/// Repo names must be repos the host knows (dirs under ~/projects/repos,
/// no '/', no leading '.'). forest checks again on its side.
fn validate_repos(caller: &Caller, repos: &[String]) -> Result<(), BridgeError> {
    let known = ls_repos(&run_forest(&caller.path, &ls_args())?)?;
    for repo in repos {
        if !known.iter().any(|k| k == repo) {
            return Err(BridgeError::new(
                "bad_request",
                format!("no repo named '{repo}' (see forest.ls)"),
            ));
        }
    }
    Ok(())
}

/// The repo names from `forest ls --json` output.
fn ls_repos(envelope: &Value) -> Result<Vec<String>, BridgeError> {
    envelope
        .get("repos")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|r| r.get("name").and_then(Value::as_str).map(String::from))
                .collect()
        })
        .ok_or_else(|| BridgeError::new("failed", "forest ls output has no repos list"))
}

/// This forest's entry in `forest ls --json` output, reshaped to the
/// bridge's result (`projects` → `repos`); Null when the path matches no
/// forest.
fn find_forest(envelope: &Value, path: &Path) -> Value {
    let Some(forests) = envelope.get("forests").and_then(Value::as_array) else {
        return Value::Null;
    };
    let me = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for f in forests {
        let Some(p) = f.get("path").and_then(Value::as_str) else {
            continue;
        };
        let p = fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
        if p == me {
            return json!({
                "name": f.get("name").cloned().unwrap_or(Value::Null),
                "path": f.get("path").cloned().unwrap_or(Value::Null),
                "branch": f.get("branch").cloned().unwrap_or(Value::Null),
                "repos": f.get("projects").and_then(Value::as_array).cloned().unwrap_or_default(),
            });
        }
    }
    Value::Null
}

// --- running the host forest binary ---------------------------------------

/// `$BENTO_FOREST_BIN`, else `forest` on PATH.
fn forest_bin() -> String {
    std::env::var("BENTO_FOREST_BIN").unwrap_or_else(|_| "forest".into())
}

/// Run the host `forest` with `--json`, argv only, and translate its
/// envelope and exit codes (exit 2 → refused). A missing binary is a
/// `failed` with a hint; a non-JSON failure keeps the stderr tail.
fn run_forest(cwd: &Path, args: &[String]) -> Result<Value, BridgeError> {
    let bin = forest_bin();
    let out = Command::new(&bin)
        .arg("--json")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            BridgeError::new(
                "failed",
                format!("running forest ({bin}): {e}; install it or set BENTO_FOREST_BIN"),
            )
        })?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if out.status.success() {
        return serde_json::from_str(stdout.trim())
            .map_err(|e| BridgeError::new("failed", format!("forest printed invalid JSON: {e}")));
    }
    if let Some((code, message)) = forest_error_envelope(&stdout) {
        let message = if message.is_empty() {
            format!("forest exited with {}", out.status)
        } else {
            message
        };
        return Err(BridgeError::new(code, message));
    }
    let code = if out.status.code() == Some(2) {
        "refused"
    } else {
        "failed"
    };
    Err(BridgeError::new(
        code,
        output_tail(&out.stderr, &out.stdout),
    ))
}

/// forest's `--json` failure envelope: `{"v":1,"error":{"code","message"}}`.
fn forest_error_envelope(stdout: &str) -> Option<(&'static str, String)> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    let e = v.get("error")?;
    let code = match e.get("code").and_then(Value::as_str) {
        Some("refused") => "refused",
        _ => "failed",
    };
    Some((
        code,
        e.get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    ))
}

/// The tail of a command's stderr (stdout as fallback): enough to see what
/// happened, not the whole log.
fn output_tail(stderr: &[u8], stdout: &[u8]) -> String {
    let text = if stderr.is_empty() { stdout } else { stderr };
    let start = text.len().saturating_sub(2048);
    String::from_utf8_lossy(&text[start..]).trim().to_string()
}

// --- forest argv builders: one small function per op, so switching a
// --- command (interim vs final) is a one-function change

/// `forest ls` (cwd: anywhere).
fn ls_args() -> Vec<String> {
    vec!["ls".into()]
}

/// decisions.md §1: `forest add [--no-fetch] <repo>...`, cwd = the forest.
/// The pre-F1 interim would be `forest new <this branch> <repos>`, but
/// that fetches origin and the bridge never fetches, so it is not wired
/// in: until F1's forest lands this fails as `refused` (unknown
/// subcommand), which is safe.
fn add_args(repos: &[String]) -> Vec<String> {
    let mut args = vec!["add".into(), "--no-fetch".into(), "--".into()];
    args.extend(repos.iter().cloned());
    args
}

/// decisions.md §3: `forest name <branch>`, cwd = the forest.
fn name_args(branch: &str) -> Vec<String> {
    vec!["name".into(), "--".into(), branch.into()]
}

/// decisions.md §1: `forest new --no-fetch --create-only <name> <repos>`
/// (`--from=<ref>`, never `--from <ref>`, so `--` keeps positionals
/// positional), cwd = the forest root's parent.
fn new_args(name: &str, repos: &[String], from: Option<&str>) -> Vec<String> {
    let mut args = vec!["new".into(), "--no-fetch".into(), "--create-only".into()];
    if let Some(from) = from {
        args.push(format!("--from={from}"));
    }
    args.push("--".into());
    args.push(name.into());
    args.extend(repos.iter().cloned());
    args
}

// --- request field helpers -------------------------------------------------

fn string_field(req: &Value, key: &str) -> Result<String, BridgeError> {
    req.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| BridgeError::new("bad_request", format!("missing string field '{key}'")))
}

fn opt_string_field(req: &Value, key: &str) -> Result<Option<String>, BridgeError> {
    match req.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(BridgeError::new(
            "bad_request",
            format!("field '{key}' must be a string"),
        )),
    }
}

fn string_list(req: &Value, key: &str) -> Result<Vec<String>, BridgeError> {
    let list = req
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| BridgeError::new("bad_request", format!("missing list field '{key}'")))?;
    list.iter()
        .map(|v| {
            v.as_str().map(String::from).ok_or_else(|| {
                BridgeError::new(
                    "bad_request",
                    format!("field '{key}' must be a list of strings"),
                )
            })
        })
        .collect()
}

/// Forest-name validation, mirroring forest's own rule (the host forest
/// checks again): no empty, no leading '.', no '/', '\\', ':' — plus the
/// branch rules, since the name is the new branch too.
fn valid_forest_name(name: &str) -> Result<(), BridgeError> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\', ':']) {
        return Err(BridgeError::new(
            "bad_request",
            format!(
                "bad forest name '{name}': must not be empty, start with '.', or contain '/', '\\' or ':'"
            ),
        ));
    }
    valid_branch(name)
}

/// Branch-name validation, forest's own check (git check-ref-format
/// --branch, no leading '-'); forest checks again on its side, so a git
/// that cannot run here does not block the op.
fn valid_branch(name: &str) -> Result<(), BridgeError> {
    if name.is_empty() || name.starts_with('-') {
        return Err(BridgeError::new(
            "bad_request",
            format!("bad branch name '{name}'"),
        ));
    }
    let ok = Command::new("git")
        .args(["check-ref-format", "--branch", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(true);
    if ok {
        Ok(())
    } else {
        Err(BridgeError::new(
            "bad_request",
            format!("bad branch name '{name}'"),
        ))
    }
}

// --- the line reader -------------------------------------------------------

#[derive(Debug)]
enum Line {
    /// A full line, newline included; at EOF without one, what there was.
    Complete(Vec<u8>),
    /// Clean EOF at a line boundary.
    Eof,
    /// More bytes than the cap, and no newline.
    TooLong,
}

/// One line from a stream, capped: a line longer than `cap` is reported
/// without reading it to the end (an endless line must not grow memory).
fn read_line(reader: &mut impl BufRead, cap: usize) -> std::io::Result<Line> {
    let mut buf = Vec::with_capacity(128);
    loop {
        let avail = match reader.fill_buf() {
            Ok(a) => a,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if avail.is_empty() {
            return Ok(if buf.is_empty() {
                Line::Eof
            } else {
                Line::Complete(buf)
            });
        }
        let room = cap - buf.len();
        let take = avail.len().min(room + 1);
        if let Some(nl) = avail[..take].iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&avail[..=nl]);
            reader.consume(nl + 1);
            return Ok(Line::Complete(buf));
        }
        if take > room {
            return Ok(Line::TooLong);
        }
        buf.extend_from_slice(&avail[..take]);
        reader.consume(take);
    }
}

// --- replies ---------------------------------------------------------------

fn ok_reply(result: Value) -> Value {
    json!({ "ok": true, "result": result })
}

fn err_reply(e: &BridgeError) -> Value {
    json!({ "ok": false, "error": { "code": e.code, "message": e.message } })
}

fn write_reply(writer: &mut TcpStream, reply: &Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(reply).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bento-bridge-test-{}-{name}", std::process::id()));
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
        // never shaped like a terminal id (t<digits>, or hc's wider id
        // alphabet: t440s, t1b, t9zzz all count)
        assert_eq!(machine_name("t12"), "t12-");
        assert_eq!(machine_name("t440s"), "t440s-");
        assert_eq!(machine_name("t1b"), "t1b-");
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

    // --- the daemon ---

    use std::net::TcpStream;
    use std::time::Duration;

    /// A runtime handle for Caller; leaked, since nothing in these tests
    /// polls it (F3 and G1 are the first who will).
    fn handle() -> tokio::runtime::Handle {
        Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()))
            .handle()
            .clone()
    }

    fn test_daemon(data_dir: &Path) -> Daemon {
        Daemon {
            data_dir: data_dir.to_path_buf(),
            rt: handle(),
        }
    }

    fn caller_at(dir: &Path) -> Caller {
        Caller {
            box_name: "bento-test".into(),
            path: dir.to_path_buf(),
            forest: workforest_name(dir),
            machine: "test".into(),
            rt: handle(),
        }
    }

    fn code_of(reply: &Value) -> &str {
        reply["error"]["code"].as_str().unwrap()
    }

    fn send(client: &mut TcpStream, line: &str) {
        client.write_all(line.as_bytes()).unwrap();
        client.write_all(b"\n").unwrap();
    }

    fn recv(client: &mut TcpStream) -> Value {
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(client.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap_or_else(|e| panic!("reading a reply: {e} ({line:?})"));
        serde_json::from_str(line.trim()).unwrap()
    }

    #[test]
    fn requests_parse_or_ask_for_an_op() {
        assert!(parse_request(br#"{"op":"hello"}"#).is_ok());
        for bad in [
            &b"not json"[..],
            br#"[1,2]"#,
            br#"{}"#,
            br#"{"op":42}"#,
            br#"{"op":""}"#,
        ] {
            let err = parse_request(bad).unwrap_err();
            assert_eq!(err.code, "bad_request", "{bad:?}");
        }
    }

    #[test]
    fn ops_need_hello_first() {
        let dir = tempdir("hello");
        let daemon = test_daemon(&dir);
        let mut caller = None;
        let err = handle_request(
            &daemon,
            &mut caller,
            "forest.ls",
            &json!({"op": "forest.ls"}),
        )
        .unwrap_err();
        assert_eq!(err.code, "hello_required");
        assert!(caller.is_none());
        // and a hello with no token does not authenticate anything
        let err =
            handle_request(&daemon, &mut caller, "hello", &json!({"op": "hello"})).unwrap_err();
        assert_eq!(err.code, "bad_request");
        assert!(caller.is_none());
    }

    #[test]
    fn tokens_are_looked_up_from_the_boxes_dir() {
        let dir = tempdir("lookup");
        for (name, token) in [("a", "aa11"), ("b", "bb22")] {
            let state = dir.join("boxes").join(name);
            fs::create_dir_all(&state).unwrap();
            fs::write(
                state.join("bridge.json"),
                serde_json::to_string(&BoxInfo {
                    token: token.into(),
                    box_name: name.into(),
                    path: "/tmp".into(),
                    port: 17761,
                    machine: name.into(),
                })
                .unwrap(),
            )
            .unwrap();
        }
        // a corrupt file (a box being removed) is skipped, not fatal
        let state = dir.join("boxes").join("c");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("bridge.json"), "{").unwrap();
        let daemon = test_daemon(&dir);
        let mut caller = None;
        let reply = handle_request(
            &daemon,
            &mut caller,
            "hello",
            &json!({"op": "hello", "token": "bb22", "client": "test"}),
        )
        .unwrap();
        assert_eq!(reply["box"], "b");
        assert!(!reply["workforest"].as_bool().unwrap());
        assert!(reply["forest"].is_null());
        let err = handle_request(
            &daemon,
            &mut caller,
            "hello",
            &json!({"op": "hello", "token": "zz99"}),
        )
        .unwrap_err();
        assert_eq!(err.code, "unknown_token");
    }

    #[test]
    fn dispatch_table_rejects_what_is_not_implemented() {
        let dir = tempdir("dispatch");
        let caller = caller_at(&dir); // not a workforest: no forests/ layout
        for (op, code, needle) in [
            ("nope", "unknown_op", "unknown op"),
            ("gpu.submit", "unknown_op", "G1"),
        ] {
            let err = dispatch(&caller, op, &json!({"op": op})).unwrap_err();
            assert_eq!(err.code, code, "{op}");
            assert!(err.message.contains(needle), "{op}: {}", err.message);
        }
        // the handoff (a brief) is F3's, and says so
        let err = dispatch(
            &caller,
            "forest.new",
            &json!({"op": "forest.new", "name": "x", "repos": ["bento"], "brief": "do it"}),
        )
        .unwrap_err();
        assert_eq!(err.code, "failed");
        assert!(err.message.contains("F3"));
    }

    #[test]
    fn forest_ops_validate_their_args() {
        let dir = tempdir("args");
        let caller = caller_at(&dir); // a checkout's box, not a forest's
                                      // add and name need a workforest box
        for req in [
            json!({"op": "forest.add", "repos": ["ois"]}),
            json!({"op": "forest.name", "branch": "x"}),
        ] {
            let op = req["op"].as_str().unwrap();
            let err = dispatch(&caller, op, &req).unwrap_err();
            assert_eq!(err.code, "not_a_forest", "{req}");
        }
        // new validates names, refs and repos before running anything
        for (req, code) in [
            (
                json!({"op": "forest.new", "name": "../x", "repos": ["bento"]}),
                "bad_request",
            ),
            (
                json!({"op": "forest.new", "name": "a:b", "repos": ["bento"]}),
                "bad_request",
            ),
            (
                json!({"op": "forest.new", "name": "ok", "repos": []}),
                "bad_request",
            ),
            (
                json!({"op": "forest.new", "name": "ok", "repos": [17]}),
                "bad_request",
            ),
            (
                json!({"op": "forest.new", "name": "ok", "repos": ["bento"], "from": "-x"}),
                "bad_request",
            ),
            (
                json!({"op": "forest.new", "repos": ["bento"]}),
                "bad_request",
            ),
        ] {
            let err = dispatch(&caller, "forest.new", &req).unwrap_err();
            assert_eq!(err.code, code, "{req}: {}", err.message);
        }
    }

    #[test]
    fn forest_argv_is_built_as_decisions_say() {
        use super::*;
        assert_eq!(
            new_args("spike-x", &["bento".into(), "ois".into()], Some("master")),
            [
                "new",
                "--no-fetch",
                "--create-only",
                "--from=master",
                "--",
                "spike-x",
                "bento",
                "ois"
            ]
        );
        assert_eq!(
            new_args("spike-x", &["bento".into()], None),
            [
                "new",
                "--no-fetch",
                "--create-only",
                "--",
                "spike-x",
                "bento"
            ]
        );
        assert_eq!(
            add_args(&["ois".into()]),
            ["add", "--no-fetch", "--", "ois"]
        );
        assert_eq!(name_args("spike/y"), ["name", "--", "spike/y"]);
        assert_eq!(ls_args(), ["ls"]);
    }

    #[test]
    fn names_validate_like_forest() {
        for good in ["spike-x", "spike/x", "exp_agent.2"] {
            assert!(valid_branch(good).is_ok(), "{good}");
        }
        for bad in ["", "-x", "a b", "a~b", "a..b", ".."] {
            assert!(valid_branch(bad).is_err(), "{bad}");
        }
        for bad in ["", ".x", "../x", "a/b", "a\\b", "a:b"] {
            assert!(valid_forest_name(bad).is_err(), "{bad}");
        }
        assert!(valid_forest_name("spike-x").is_ok());
    }

    #[test]
    fn find_forest_reshapes_the_ls_entry() {
        let dir = tempdir("ff");
        let envelope = json!({
            "forests": [
                {"name": "other", "path": "/definitely/not/here", "branch": "o", "projects": ["x"]},
                {"name": "mine", "path": dir.display().to_string(), "branch": "m", "projects": ["a", "b"]},
            ]
        });
        let f = find_forest(&envelope, &dir);
        assert_eq!(f["name"], "mine");
        assert_eq!(f["branch"], "m");
        assert_eq!(f["repos"], json!(["a", "b"]));
        assert_eq!(find_forest(&envelope, &dir.join("nope")), Value::Null);
        assert_eq!(find_forest(&json!({}), &dir), Value::Null);
    }

    #[test]
    fn error_replies_have_the_wire_shape() {
        let reply = err_reply(&BridgeError::new("refused", "no"));
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], "refused");
        assert_eq!(reply["error"]["message"], "no");
        let reply = ok_reply(json!({"a": 1}));
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["result"]["a"], 1);
    }

    #[test]
    fn forest_failure_envelopes_translate() {
        assert_eq!(
            forest_error_envelope(r#"{"v":1,"error":{"code":"refused","message":"no"}}"#),
            Some(("refused", "no".to_string()))
        );
        assert_eq!(
            forest_error_envelope(r#"{"v":1,"error":{"code":"error","message":"boom"}}"#),
            Some(("failed", "boom".to_string()))
        );
        assert_eq!(forest_error_envelope("usage: forest ..."), None);
    }

    #[test]
    fn output_tails_stay_bounded() {
        let stderr: Vec<u8> = std::iter::repeat_n(b'x', 10_000).collect();
        assert_eq!(output_tail(&stderr, b"").len(), 2048);
        // stderr empty: the stdout tail stands in
        assert_eq!(
            output_tail(b"", b"use stderr only when empty\n"),
            "use stderr only when empty"
        );
    }

    #[test]
    fn lines_are_read_capped() {
        let mut r = std::io::Cursor::new(b"{\"op\":1}\nnext".to_vec());
        match read_line(&mut r, 100).unwrap() {
            Line::Complete(b) => assert_eq!(b, b"{\"op\":1}\n"),
            other => panic!("{other:?}"),
        }
        // EOF mid-line hands back what there is: a bad request upstream
        match read_line(&mut r, 100).unwrap() {
            Line::Complete(b) => assert_eq!(b, b"next"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(read_line(&mut r, 100).unwrap(), Line::Eof));
        // exactly at the cap is a line; past it is not, without reading on
        let mut r = std::io::Cursor::new(b"0123456789\n".to_vec());
        assert!(matches!(read_line(&mut r, 10).unwrap(), Line::Complete(_)));
        let mut r = std::io::Cursor::new(vec![b'a'; 50]);
        assert!(matches!(read_line(&mut r, 10).unwrap(), Line::TooLong));
    }

    #[test]
    fn the_protocol_over_a_real_socket() {
        let dir = tempdir("sock");
        let root = dir.join("root");
        let forest_dir = root.join("forests").join("thing");
        fs::create_dir_all(root.join("repos")).unwrap();
        fs::create_dir_all(&forest_dir).unwrap();
        let info = BoxInfo {
            token: "ab".repeat(16),
            box_name: "bento-forest_thing".into(),
            path: forest_dir.display().to_string(),
            port: 17761,
            machine: "thing".into(),
        };
        let state = dir.join("boxes").join(&info.box_name);
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join("bridge.json"),
            serde_json::to_string(&info).unwrap(),
        )
        .unwrap();
        let daemon = test_daemon(&dir);
        // port 0: whatever the OS gives, never the real daemon's
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_conn(&daemon, stream);
        });
        let mut client = TcpStream::connect(addr).unwrap();

        // op before hello
        send(&mut client, r#"{"op":"forest.ls"}"#);
        assert_eq!(code_of(&recv(&mut client)), "hello_required");
        // unknown token
        send(&mut client, r#"{"op":"hello","token":"00"}"#);
        assert_eq!(code_of(&recv(&mut client)), "unknown_token");
        // invalid JSON
        send(&mut client, "not json");
        assert_eq!(code_of(&recv(&mut client)), "bad_request");
        // hello: the shape decisions §1 promises
        send(
            &mut client,
            &format!(
                r#"{{"op":"hello","token":"{}","client":"test"}}"#,
                info.token
            ),
        );
        let reply = recv(&mut client);
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["result"]["box"], "bento-forest_thing");
        assert_eq!(reply["result"]["forest"], "thing");
        assert_eq!(reply["result"]["workforest"], true);
        assert_eq!(reply["result"]["path"], forest_dir.display().to_string());
        // a second hello re-authenticates (idempotent)
        send(
            &mut client,
            &format!(r#"{{"op":"hello","token":"{}"}}"#, info.token),
        );
        assert_eq!(recv(&mut client)["ok"], true);
        // blank lines are skipped
        send(&mut client, "");
        send(&mut client, r#"{"op":"nope"}"#);
        assert_eq!(code_of(&recv(&mut client)), "unknown_op");

        // an oversized line is refused and the connection closed
        let mut huge = vec![b'a'; MAX_LINE + 10];
        huge.push(b'\n');
        let _ = client.write_all(&huge); // may hit EPIPE once the daemon hangs up
        let mut buf = [0u8; 4096];
        let mut settled = false;
        for _ in 0..50 {
            match client.read(&mut buf) {
                Ok(0) | Err(_) => {
                    settled = true;
                    break;
                }
                Ok(n) if String::from_utf8_lossy(&buf[..n]).contains("bad_request") => {
                    settled = true;
                    break;
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        assert!(settled, "the daemon must hang up on an oversized line");
    }
}
