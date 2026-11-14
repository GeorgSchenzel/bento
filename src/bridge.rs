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

/// hc caps machine names at 32 chars (addr.rs `clean_name`).
pub const MAX_MACHINE: usize = 32;
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
    match env_port()? {
        Some(p) => Ok(p),
        None => Ok(DEFAULT_PORT),
    }
}

/// `BENTO_BRIDGE_PORT` as `Some(port)`, `None` when unset. Callers decide
/// whether an unset var means the default or "keep what is stored".
fn env_port() -> Result<Option<u16>> {
    match std::env::var("BENTO_BRIDGE_PORT") {
        Ok(v) => v
            .parse()
            .map(Some)
            .with_context(|| format!("bad BENTO_BRIDGE_PORT '{v}'")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(e).context("reading BENTO_BRIDGE_PORT"),
    }
}

/// The box's bridge.json, read or created. The token is fixed once per box
/// and reused, so it survives box recreation (`bento start`) without the
/// env plumbing changing under a running setup.
///
/// The port follows the daemon when the caller knows it: an explicit
/// `BENTO_BRIDGE_PORT` rewrites a stored port that differs, so a moved
/// daemon reaches its boxes after their next `bento start`. Without the
/// env var the stored port stays as it is; when it is not the default, a
/// warning says what the box expects — exactly the case where starting
/// `bento serve` plain makes every box look offline.
pub fn ensure_box_info(state: &Path, box_name: &str, dir: &Path) -> Result<BoxInfo> {
    let path = state.join("bridge.json");
    let port = env_port()?;
    if let Ok(text) = fs::read_to_string(&path) {
        if let Ok(mut info) = serde_json::from_str::<BoxInfo>(&text) {
            match port {
                Some(p) if p != info.port => {
                    eprintln!(
                        "bento: box '{box_name}' bridge port {} -> {p} (BENTO_BRIDGE_PORT); \
                         `bento start` applies it to a running box",
                        info.port
                    );
                    info.port = p;
                    write_secret(&path, &serde_json::to_vec_pretty(&info)?)?;
                }
                Some(_) => {}
                None if info.port != DEFAULT_PORT => {
                    eprintln!(
                        "bento: box '{box_name}' expects the bridge on port {} (not the default {}); \
                         start `bento serve --port {}` or export BENTO_BRIDGE_PORT={}",
                        info.port, DEFAULT_PORT, info.port, info.port
                    );
                }
                None => {}
            }
            return Ok(info);
        }
        // unreadable or corrupt: fall through and regenerate (a fresh token
        // invalidates nothing — the daemon reads the same file)
    }
    let info = BoxInfo {
        token: random_token().context("generating the bridge token")?,
        box_name: box_name.to_string(),
        path: dir.display().to_string(),
        port: port.unwrap_or(DEFAULT_PORT),
        machine: unique_machine(state, &dir_name(dir))?,
    };
    write_secret(&path, &serde_json::to_vec_pretty(&info)?)?;
    Ok(info)
}

/// The machine name for a new box: the derived name unless another box's
/// bridge.json claims it, then `-2`, `-3`, … — hc routes by name, so a
/// collision would let one box's registration remove another's machine
/// (M2). The base is truncated to keep the suffixed name inside hc's 32
/// chars; the suffix also breaks terminal-id shapes (`-` is not in hc's
/// id alphabet). Sibling state dirs with unreadable bridge.json files are
/// skipped: the worst case is a duplicate, the same as before.
fn unique_machine(state: &Path, dir_name: &str) -> Result<String> {
    let base = machine_name(dir_name);
    let taken = machines_of_other_boxes(state);
    if !taken.contains(&base) {
        return Ok(base);
    }
    for n in 2u32.. {
        let suffix = format!("-{n}");
        // machine_name output is ASCII, so cutting bytes cuts chars
        let cut = MAX_MACHINE.saturating_sub(suffix.len());
        let candidate = format!("{}{suffix}", &base[..base.len().min(cut)]);
        if !taken.contains(&candidate) {
            return Ok(candidate);
        }
        if n > 9_999 {
            return Err(anyhow!(
                "no free machine name for '{}' after {n} boxes",
                dir_name
            ));
        }
    }
    unreachable!()
}

/// Which machine name another box's bridge.json claims: the claiming box's
/// name, when there is one. `own_state` (this box's state dir) is excluded;
/// `machine` is checked as-is, so a pre-uniqueness duplicate is visible to
/// both sides. Used by herdr's register/unregister to never remove a
/// machine another box still owns.
pub fn machine_owner(own_state: &Path, machine: &str) -> Option<String> {
    machines_with_owners(own_state)
        .into_iter()
        .find(|(m, _)| m == machine)
        .map(|(_, owner)| owner)
}

fn machines_with_owners(own_state: &Path) -> Vec<(String, String)> {
    let Some(boxes) = own_state.parent() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in fs::read_dir(boxes).ok().into_iter().flatten().flatten() {
        let state = entry.path();
        if state == own_state {
            continue;
        }
        let Ok(text) = fs::read_to_string(state.join("bridge.json")) else {
            continue; // a box being removed, or mid-write
        };
        let Ok(info) = serde_json::from_str::<BoxInfo>(&text) else {
            continue; // corrupt: never fatal
        };
        found.push((info.machine, info.box_name));
    }
    found
}

fn machines_of_other_boxes(state: &Path) -> std::collections::BTreeSet<String> {
    machines_with_owners(state)
        .into_iter()
        .map(|(m, _)| m)
        .collect()
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
        .take(MAX_MACHINE)
        .collect();
    if name.is_empty() {
        return "box".into();
    }
    if id_shaped(&name) {
        if name.len() >= MAX_MACHINE {
            name.truncate(MAX_MACHINE - 1);
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

/// `bento serve`'s port, made the process's `BENTO_BRIDGE_PORT` so every
/// box created in this process (handoffs) records the reachable port.
pub(crate) fn adopt_serve_port(port: u16) {
    std::env::set_var("BENTO_BRIDGE_PORT", port.to_string());
}

/// An error as it crosses the wire: `{"ok":false,"error":{code,message}}`.
/// pub(crate): the GPU queue (src/gpu.rs) builds the same errors.
#[derive(Debug)]
pub(crate) struct BridgeError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl BridgeError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
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
    #[allow(dead_code)] // read by ois, not by any bridge op yet
    pub machine: String,
    /// The daemon's runtime handle: a handler that must call bento's async
    /// code (F3's handoff creates a box) uses it from its own thread.
    pub rt: tokio::runtime::Handle,
}

struct Daemon {
    data_dir: PathBuf,
    rt: tokio::runtime::Handle,
    /// The GPU job queue (decisions §6); started with the daemon.
    gpu: crate::gpu::Queue,
}

/// Run the bridge daemon until killed. Started by the user (a systemd
/// --user unit later); nothing auto-starts it. Starting it also starts the
/// GPU job queue (one worker, one job at a time — decisions §6).
pub async fn serve(port: u16) -> Result<()> {
    // The port this daemon listens on becomes the process's
    // BENTO_BRIDGE_PORT (M3): boxes created while it runs — a handoff's
    // `forest.new` — read it through ensure_box_info, and a `--port` the
    // env never knew must not leave them pointing at the default.
    adopt_serve_port(port);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .with_context(|| format!("binding 127.0.0.1:{port}"))?;
    let daemon = Arc::new(Daemon {
        data_dir: util::data_dir()?,
        rt: tokio::runtime::Handle::current(),
        gpu: crate::gpu::Queue::start()?,
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
    dispatch(daemon, c, op, req)
}

/// The dispatch table: one arm per op (decisions §1). The GPU arms hand the
/// whole request to the queue (src/gpu.rs), which owns the safety rules.
/// gpu.* needs a workforest box (M5): a single checkout's box would jail the
/// repo root itself, `.git` included.
fn dispatch(daemon: &Daemon, caller: &Caller, op: &str, req: &Value) -> Result<Value, BridgeError> {
    if let Some(gpu_op) = op.strip_prefix("gpu.") {
        if matches!(gpu_op, "submit" | "status" | "ls") {
            require_workforest(caller)?;
        }
        return match gpu_op {
            "submit" => daemon.gpu.submit(caller, req),
            "status" => daemon.gpu.status(caller, req),
            "ls" => daemon.gpu.ls(caller),
            _ => Err(BridgeError::new(
                "unknown_op",
                format!("unknown op '{op}' (gpu.submit, gpu.status, gpu.ls)"),
            )),
        };
    }
    match op {
        "forest.ls" => forest_ls(caller),
        "forest.add" => forest_add(caller, req),
        "forest.name" => forest_name(caller, req),
        "forest.new" => forest_new(caller, req),
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
/// all; with one it becomes a handoff (§5): the new forest gets a box and
/// its first agent. Always `--no-fetch --create-only`: fetching is the
/// user's, and never join an existing forest (e.g. the user's).
fn forest_new(caller: &Caller, req: &Value) -> Result<Value, BridgeError> {
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
    let brief = match opt_string_field(req, "brief")? {
        Some(b) if b.trim().is_empty() => {
            return Err(BridgeError::new("bad_request", "'brief' must not be empty"));
        }
        b => b,
    };
    validate_repos(caller, &repos)?;
    // any cwd works (forest resolves its root); the forest root's parent
    // keeps `forest new` away from this box's own forest
    let cwd = caller.path.parent().unwrap_or(&caller.path).to_path_buf();
    let envelope = run_forest(&cwd, &new_args(&name, &repos, from.as_deref()))?;
    match brief {
        Some(brief) => {
            let path = PathBuf::from(
                envelope
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| BridgeError::new("failed", "forest new output has no path"))?,
            );
            handoff(caller, &name, &repos, &path, &brief)
        }
        None => Ok(json!({
            "name": envelope.get("forest").cloned().unwrap_or(Value::Null),
            "path": envelope.get("path").cloned().unwrap_or(Value::Null),
        })),
    }
}

/// `forest.new` with a brief: everything past the forest itself (decisions
/// §5). Steps 2-4 — the box (which registers the machine, §4), then the
/// first agent on it. A failure names the step and what exists, because
/// nothing is deleted automatically: the user decides.
fn handoff(
    caller: &Caller,
    name: &str,
    repos: &[String],
    path: &Path,
    brief: &str,
) -> Result<Value, BridgeError> {
    // 2. the box, through the same code path `bento start` takes, on the
    // daemon's runtime (the connection handler is a plain thread)
    let forest = crate::forest::at(path)
        .map_err(|e| handoff_error("reading the new forest", &e, name, path, None, None))?;
    let box_name = forest.name.clone();
    if let Err(e) = caller.rt.block_on(crate::sandbox::create(&forest)) {
        let created = caller.rt.block_on(async {
            microsandbox::sandbox::Sandbox::get(&forest.name)
                .await
                .is_ok()
        });
        return Err(handoff_error(
            "creating its box",
            &e,
            name,
            path,
            Some((&box_name, created)),
            None,
        ));
    }
    // 3. the machine: registered by the same create (warns on failure);
    // everyone reads the name from the box's bridge identity (§4)
    let machine = match sandbox_machine(&forest) {
        Ok(m) => m,
        Err(e) => {
            return Err(handoff_error(
                "reading the box's bridge identity",
                &e,
                name,
                path,
                Some((&box_name, true)),
                None,
            ))
        }
    };
    if !crate::herdr::configured() {
        return Ok(json!({
            "name": name,
            "path": path.display().to_string(),
            "box": box_name,
            "warning": "no hc core configured; start an agent with `bento opencode`",
        }));
    }
    // 4. the first agent: `hc run --bg --name lead --cwd /workspace opencode`
    let task = handoff_brief(caller, name, repos, brief);
    match crate::herdr::handoff_lead(&machine, &task) {
        Ok(terminal) => Ok(json!({
            "name": name,
            "path": path.display().to_string(),
            "box": box_name,
            "machine": machine,
            "terminal": terminal,
        })),
        Err(e) => Err(handoff_error(
            "starting its first agent",
            &e,
            name,
            path,
            Some((&box_name, true)),
            Some(&machine),
        )),
    }
}

/// The brief the lead agent gets: a header naming the handoff — who asked,
/// what the forest holds — then the requester's task. The header also
/// guarantees the text never starts with a flag (`hc run` parses those).
fn handoff_brief(caller: &Caller, name: &str, repos: &[String], brief: &str) -> String {
    let from = caller
        .forest
        .clone()
        .unwrap_or_else(|| caller.box_name.clone());
    format!(
        "Handoff from forest {from} (agent request). Your forest: {name}, repos {}. Task:\n\n{}",
        repos.join(", "),
        brief.trim()
    )
}

/// What a handoff error says: the step that failed, the cause, and what
/// exists now — nothing is removed behind the caller's back.
fn handoff_error(
    step: &str,
    cause: &anyhow::Error,
    name: &str,
    path: &Path,
    box_name: Option<(&str, bool)>,
    machine: Option<&str>,
) -> BridgeError {
    let what_exists = format!(
        "forest '{name}' at {} (kept; nothing was deleted), box {}, machine {}",
        path.display(),
        match box_name {
            Some((b, true)) => format!("'{b}' created"),
            Some((b, false)) => format!("'{b}' not created"),
            None => "not reached".into(),
        },
        match machine {
            Some(m) => format!("'{m}'"),
            None => "not reached".into(),
        },
    );
    BridgeError::new(
        "failed",
        format!(
            "the handoff stopped at '{step}': {cause:#}. Exists now: {what_exists}. \
             Finish it by hand (`bento start` in {path}, then `hc run --bg --on <machine> \
             --name lead --cwd /workspace opencode \"<brief>\"`) or remove what you do not want",
            path = path.display(),
        ),
    )
}

/// The new box's machine name, from the bridge identity create just wrote
/// (§4: bridge.json is the one record everyone reads).
fn sandbox_machine(forest: &crate::forest::Forest) -> Result<String, anyhow::Error> {
    let state = crate::sandbox::state_dir(forest)?;
    Ok(ensure_box_info(&state, &forest.name, &forest.dir)?.machine)
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

/// pub(crate): the GPU queue reads request fields with the same errors.
pub(crate) fn string_field(req: &Value, key: &str) -> Result<String, BridgeError> {
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

    /// Tests that read or write BENTO_BRIDGE_PORT hold this: cargo runs
    /// the tests of one binary in parallel threads with one environment.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Poisoned only by a panicked holder; the next test can still run.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

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

    /// A second box of the same dir name gets a suffixed machine (M2): hc
    /// routes by name, and a duplicate would let one box's registration
    /// remove another's machine. The suffix stays inside hc's 32 chars and
    /// breaks terminal-id shapes.
    #[test]
    fn machine_names_are_unique_across_boxes() {
        let _env = lock_env();
        std::env::remove_var("BENTO_BRIDGE_PORT");
        let root = tempdir("uniq");
        let (boxes, dirs) = (root.join("boxes"), root.join("dirs"));
        fs::create_dir_all(&boxes).unwrap();
        let dir_of = |n: &str| {
            let d = dirs.join(n);
            fs::create_dir_all(&d).unwrap();
            d
        };
        let box_state = |n: &str| {
            let s = boxes.join(n);
            fs::create_dir_all(&s).unwrap();
            s
        };
        let first = ensure_box_info(&box_state("b1"), "bento-b1", &dir_of("thing")).unwrap();
        assert_eq!(first.machine, "thing");
        let second = ensure_box_info(&box_state("b2"), "bento-b2", &dir_of("thing")).unwrap();
        assert_eq!(second.machine, "thing-2");
        let third = ensure_box_info(&box_state("b3"), "bento-b3", &dir_of("Thing")).unwrap();
        assert_eq!(third.machine, "thing-3"); // case is not a difference

        // a long name: the base is truncated so base+suffix fits in 32
        let long = "l".repeat(40);
        let a = ensure_box_info(&box_state("b4"), "bento-b4", &dir_of(&long)).unwrap();
        assert_eq!(a.machine.len(), 32);
        let b = ensure_box_info(&box_state("b5"), "bento-b5", &dir_of(&long)).unwrap();
        assert_eq!(b.machine.len(), 32, "suffixed name must stay within 32");
        assert_ne!(a.machine, b.machine);
        assert_eq!(&b.machine[..30], &a.machine[..30]);

        // id-shaped names stay non-id-shaped with a suffix
        let c = ensure_box_info(&box_state("b6"), "bento-b6", &dir_of("t12")).unwrap();
        assert_eq!(c.machine, "t12-");
        let d = ensure_box_info(&box_state("b7"), "bento-b7", &dir_of("t12")).unwrap();
        assert_eq!(d.machine, "t12--2");
        assert!(!id_shaped(&d.machine));

        // a box with a corrupt bridge.json does not block a name
        fs::create_dir_all(boxes.join("broken")).unwrap();
        fs::write(boxes.join("broken/bridge.json"), "{").unwrap();
        let e = ensure_box_info(&box_state("b8"), "bento-b8", &dir_of("fresh")).unwrap();
        assert_eq!(e.machine, "fresh");
        fs::remove_dir_all(&root).unwrap();
    }

    /// M3: the port a `bento serve --port N` daemon runs on is adopted as
    /// the process's BENTO_BRIDGE_PORT, so a box created while it runs (a
    /// handoff's forest.new) records N in bridge.json — not the default
    /// the env never mentioned.
    #[test]
    fn serve_adopts_its_port_for_boxes_created_later() {
        let _env = lock_env();
        std::env::remove_var("BENTO_BRIDGE_PORT");
        let root = tempdir("serve-port");
        let dir = root.join("thing");
        fs::create_dir_all(&dir).unwrap();
        let state = root.join("boxes/x");
        fs::create_dir_all(&state).unwrap();
        adopt_serve_port(17780);
        let info = ensure_box_info(&state, "bento-x", &dir).unwrap();
        assert_eq!(info.port, 17780);
        // later readers without the env still see the stored port, token kept
        std::env::remove_var("BENTO_BRIDGE_PORT");
        let again = ensure_box_info(&state, "bento-x", &dir).unwrap();
        assert_eq!(again.port, 17780);
        assert_eq!(again.token, info.token);
        fs::remove_dir_all(&root).unwrap();
    }

    /// M4: an explicit BENTO_BRIDGE_PORT updates an existing bridge.json
    /// (the daemon moved); without it the stored port stays; a bad value is
    /// an error even when the file is there, and never changes it.
    #[test]
    fn an_explicit_port_follows_the_daemon() {
        let _env = lock_env();
        std::env::remove_var("BENTO_BRIDGE_PORT");
        let root = tempdir("port-follow");
        let dir = root.join("thing");
        fs::create_dir_all(&dir).unwrap();
        let state = root.join("boxes/x");
        fs::create_dir_all(&state).unwrap();
        let first = ensure_box_info(&state, "bento-x", &dir).unwrap();
        assert_eq!(first.port, DEFAULT_PORT);

        std::env::set_var("BENTO_BRIDGE_PORT", "17800");
        let second = ensure_box_info(&state, "bento-x", &dir).unwrap();
        assert_eq!(second.port, 17800);
        assert_eq!(second.token, first.token, "only the port is rewritten");

        std::env::remove_var("BENTO_BRIDGE_PORT");
        let third = ensure_box_info(&state, "bento-x", &dir).unwrap();
        assert_eq!(third.port, 17800, "without the env, the stored port stays");

        std::env::set_var("BENTO_BRIDGE_PORT", "not-a-port");
        let err = ensure_box_info(&state, "bento-x", &dir).unwrap_err();
        assert!(err.to_string().contains("BENTO_BRIDGE_PORT"), "{err:#}");
        std::env::remove_var("BENTO_BRIDGE_PORT");
        let fourth = ensure_box_info(&state, "bento-x", &dir).unwrap();
        assert_eq!(fourth.port, 17800, "a bad value leaves the file alone");
        fs::remove_dir_all(&root).unwrap();
    }

    /// The M2 guard herdr's register/unregister use: a machine another
    /// box's bridge.json claims has that box's name on it — whichever side
    /// asks.
    #[test]
    fn machine_owner_names_the_other_box() {
        let boxes = tempdir("owner").join("boxes");
        for (name, machine) in [("a", "thing"), ("b", "thing"), ("c", "other")] {
            let state = boxes.join(name);
            fs::create_dir_all(&state).unwrap();
            fs::write(
                state.join("bridge.json"),
                serde_json::to_string(&BoxInfo {
                    token: format!("t-{name}"),
                    box_name: format!("bento-{name}"),
                    path: "/tmp".into(),
                    port: 17761,
                    machine: machine.into(),
                })
                .unwrap(),
            )
            .unwrap();
        }
        assert_eq!(
            machine_owner(&boxes.join("a"), "thing").as_deref(),
            Some("bento-b")
        );
        assert_eq!(
            machine_owner(&boxes.join("b"), "thing").as_deref(),
            Some("bento-a")
        );
        // own claims are invisible, unknown machines have no owner
        assert_eq!(machine_owner(&boxes.join("c"), "other"), None);
        assert_eq!(machine_owner(&boxes.join("a"), "nope"), None);
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
        let _env = lock_env();
        std::env::remove_var("BENTO_BRIDGE_PORT");
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempdir("state");
        // under a boxes/ dir, like the real layout: the uniqueness scan
        // reads the state dir's siblings
        let state = root.join("boxes/bento-test");
        fs::create_dir_all(&state).unwrap();
        let dir = root.join("tree");
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
        fs::remove_dir_all(&root).unwrap();
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
            // a real queue on a temp state dir: validation paths work, and no
            // test submits a runnable job (the worker stays idle)
            gpu: crate::gpu::Queue::start_at(&data_dir.join("gpu-state")).unwrap(),
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
        let daemon = test_daemon(&dir);
        let caller = caller_at(&dir); // not a workforest: no forests/ layout
        for (op, code, needle) in [
            ("nope", "unknown_op", "unknown op"),
            ("gpu.magic", "unknown_op", "gpu.submit"),
        ] {
            let err = dispatch(&daemon, &caller, op, &json!({"op": op})).unwrap_err();
            assert_eq!(err.code, code, "{op}");
            assert!(err.message.contains(needle), "{op}: {}", err.message);
        }
        // a brief now routes into the handoff (F3); before any forest runs,
        // the same validations answer (here: a bad name)
        let err = dispatch(
            &daemon,
            &caller,
            "forest.new",
            &json!({"op": "forest.new", "name": "a:b", "repos": ["bento"], "brief": "do it"}),
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
        // an empty brief never starts a half-told agent
        let err = dispatch(
            &daemon,
            &caller,
            "forest.new",
            &json!({"op": "forest.new", "name": "ok", "repos": ["bento"], "brief": "  "}),
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
        assert!(err.message.contains("brief"), "{}", err.message);
    }

    #[test]
    fn gpu_ops_need_a_workforest_box() {
        let dir = tempdir("gpu-forest");
        // a single checkout box: .git file + a real subdir — not a
        // forests/<name> layout. Its root IS the repo, so a jail for it
        // would bind the whole repo including .git: gpu.* must refuse.
        let checkout = dir.join("bento");
        fs::create_dir_all(checkout.join("sub")).unwrap();
        fs::write(checkout.join(".git"), "gitdir: fake").unwrap();
        let daemon = test_daemon(&dir);
        let caller = caller_at(&checkout);
        assert_eq!(caller.forest, None, "fixture must be a non-workforest box");
        // cwd "sub" is a real dir inside the checkout, so this submit passes
        // path validation — the refusal must come from the workforest rule
        let err = dispatch(
            &daemon,
            &caller,
            "gpu.submit",
            &json!({"op":"gpu.submit","cwd":"sub","cmd":"true"}),
        )
        .unwrap_err();
        assert_eq!(err.code, "not_a_forest", "{}", err.message);
        for (op, req) in [
            ("gpu.status", json!({"op":"gpu.status","id":"20261113-000000-0000"})),
            ("gpu.ls", json!({"op":"gpu.ls"})),
        ] {
            let err = dispatch(&daemon, &caller, op, &req).unwrap_err();
            assert_eq!(err.code, "not_a_forest", "{op}: {}", err.message);
        }
    }

    #[test]
    fn forest_ops_validate_their_args() {
        let dir = tempdir("args");
        let daemon = test_daemon(&dir);
        let caller = caller_at(&dir); // a checkout's box, not a forest's
                                      // add and name need a workforest box
        for req in [
            json!({"op": "forest.add", "repos": ["ois"]}),
            json!({"op": "forest.name", "branch": "x"}),
        ] {
            let op = req["op"].as_str().unwrap();
            let err = dispatch(&daemon, &caller, op, &req).unwrap_err();
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
            let err = dispatch(&daemon, &caller, "forest.new", &req).unwrap_err();
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

    /// The lead agent's brief: the §5 header (who asked, what the forest
    /// is), then the requester's task — and never flag-shaped at the start.
    #[test]
    fn handoff_briefs_carry_the_header() {
        let mut caller = caller_at(&tempdir("hb"));
        caller.forest = Some("spike-F3-handoff".into());
        let brief = handoff_brief(
            &caller,
            "spike-f3-made",
            &["bento".into()],
            "write ok and stop",
        );
        assert_eq!(
            brief,
            "Handoff from forest spike-F3-handoff (agent request). Your forest: \
             spike-f3-made, repos bento. Task:\n\nwrite ok and stop"
        );
        assert!(!brief.starts_with('-'), "must not look like an hc flag");
        // a checkout's box (no forest) is named by its box instead
        caller.forest = None;
        assert!(handoff_brief(&caller, "x", &["a".into(), "b".into()], "t")
            .starts_with("Handoff from forest bento-test (agent request)."));
    }

    /// A half-made handoff says which step failed and what exists.
    #[test]
    fn handoff_errors_report_the_leftovers() {
        let err = handoff_error(
            "creating its box",
            &anyhow!("boom"),
            "spike-f3-made",
            Path::new("/home/g/projects/forests/spike-f3-made"),
            Some(("bento-forest_spike-f3-made", false)),
            None,
        );
        assert_eq!(err.code, "failed");
        let msg = err.message;
        assert!(msg.contains("stopped at 'creating its box'"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
        assert!(msg.contains("kept; nothing was deleted"), "{msg}");
        assert!(
            msg.contains("'bento-forest_spike-f3-made' not created"),
            "{msg}"
        );
        assert!(msg.contains("machine not reached"), "{msg}");
        let err = handoff_error(
            "starting its first agent",
            &anyhow!("nope"),
            "n",
            Path::new("/p"),
            Some(("b", true)),
            Some("n"),
        );
        assert!(err.message.contains("'b' created"), "{}", err.message);
        assert!(err.message.contains("machine 'n'"), "{}", err.message);
        assert!(err.message.contains("hc run --bg"), "{}", err.message);
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
