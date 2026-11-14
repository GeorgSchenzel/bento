//! The host GPU job queue (decisions §6): one job at a time on the host, FIFO
//! across all forests, jailed by `gpujail.sh` (embedded from assets/, written
//! to the state dir when `bento serve` starts).
//!
//! A job is a JSON file under `$BENTO_GPU_STATE/{queued,running,done}/`
//! (claim by rename, so a crash never runs a job twice). Its results live in
//! TWO places (B1):
//!
//! - `$BENTO_GPU_STATE/out/<id>/` — the AUTHORITATIVE `cmd`, `stdout`,
//!   `stderr`, `exit`, `timed-out`, written by the daemon alone. `gpu.status`
//!   tails from here. The state dir is not inside any forest, so no box and
//!   no job can reach it.
//! - `<forest>/<checkout>/.gpu-jobs/<id>/` — the COPY the box agent reads
//!   (`/workspace/<tree>/.gpu-jobs/<id>`). The checkout (and everything
//!   under it) is writable by the box AND by the job, so the copy is done
//!   with openat + O_NOFOLLOW at every hop and O_CREAT|O_EXCL for files
//!   (see `sys` and `copy_results`): a symlink planted anywhere on that
//!   walk skips the whole copy — recorded in the job record, surfaced by
//!   `gpu.status` — and is never followed. `files/` alone is written by the
//!   job inside the jail (the jail can only reach the forest anyway).
//!
//! Safety (decisions §1 "Safety rules for ops"): the submitted `cwd` must
//! resolve (realpath) inside a checkout of the caller's own forest — no `..`,
//! no absolute paths, no symlinked `.gpu-jobs` or job dirs; status/ls only
//! ever show the caller's own jobs. The job command is passed to gpujail.sh
//! as argv (`bash -lc <cmd>`); no host shell ever sees it.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::bridge::{string_field, BridgeError, Caller};
use crate::util;

mod sys {
    //! Thin libc wrappers for symlink-refusing file operations (B1): every
    //! directory hop is `openat` with `O_DIRECTORY|O_NOFOLLOW`, files are
    //! created with `O_CREAT|O_EXCL|O_NOFOLLOW`. A symlink anywhere on the
    //! walk fails (ELOOP) instead of being followed.
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io;
    use std::io::Write as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::io::{AsRawFd as _, FromRawFd as _};
    use std::path::Path;

    fn cstr(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "the path contains a NUL byte")
        })
    }

    fn flags_dir() -> libc::c_int {
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
    }

    /// Open `name` under the dir `parent` (an absolute path when `parent` is
    /// None) as a real directory, refusing symlinks.
    pub fn open_dir_at(parent: Option<&File>, name: &OsStr) -> io::Result<File> {
        let c = cstr(name)?;
        let fd = unsafe {
            libc::openat(
                parent.map_or(libc::AT_FDCWD, |p| p.as_raw_fd()),
                c.as_ptr(),
                flags_dir(),
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    /// Create `name` under `parent` (mkdir never follows the final
    /// component), then open it as a dir without following.
    pub fn mkdir_at(parent: &File, name: &OsStr) -> io::Result<File> {
        let c = cstr(name)?;
        let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), c.as_ptr(), 0o755) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        open_dir_at(Some(parent), name)
    }

    /// Create `name` under `parent` with `bytes` (mode 0644), refusing to
    /// follow or clobber anything that already exists.
    pub fn create_exclusive_at(parent: &File, name: &str, bytes: &[u8]) -> io::Result<()> {
        let c = CString::new(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in name"))?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                c.as_ptr(),
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC,
                0o644,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(bytes)?;
        Ok(())
    }

    /// Open a file for reading, refusing symlinks (tails and other reads of
    /// anything a box could have touched).
    pub fn open_read_nofollow(path: &Path) -> io::Result<File> {
        let c = cstr(path.as_os_str())?;
        let fd = unsafe {
            libc::openat(
                libc::AT_FDCWD,
                c.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }
}


/// R5's jail, embedded: written to the state dir (0755) when the queue starts.
const GPUJAIL: &str = include_str!("../assets/gpujail.sh");

/// Default timeout (decisions §1: default 600, max 3600).
const DEFAULT_TIMEOUT_S: u64 = 600;
const MAX_TIMEOUT_S: u64 = 3600;

/// Tails returned by gpu.status: the last 64 KiB, enough to read a result.
const TAIL: u64 = 64 * 1024;

/// The "runner died" exit code (R5's convention).
const RUNNER_DIED: i32 = 250;

// ---------------------------------------------------------------------------
// State layout and job records
// ---------------------------------------------------------------------------

/// `$BENTO_GPU_STATE`, else `~/.local/share/bento/gpu/` (decisions §6).
pub fn state_dir() -> anyhow::Result<PathBuf> {
    match std::env::var_os("BENTO_GPU_STATE") {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(util::data_dir()?.join("gpu")),
    }
}

/// The daemon-private, AUTHORITATIVE results dir for a job id (B1): no box
/// and no job can write here, so the daemon's own writes need no defense —
/// and every read of checkout files can be skipped in favour of these.
fn out_dir(state: &Path, id: &str) -> PathBuf {
    state.join("out").join(id)
}

/// One job, as persisted under `{queued,running,done}/<id>.json`. Fields a
/// box could not know better (forest paths) are set by the daemon from the
/// caller's identity, never from the request.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Job {
    /// `<YYYYMMDD-HHMMSS>-<4 hex>`: lexicographic order = submission order.
    id: String,
    /// The caller's root: the workforest dir, or the single checkout.
    forest: String,
    /// The checkout dir (host path) that holds this job's results.
    checkout: String,
    /// The checkout's dir name — the box mounts it at `/workspace/<tree>`.
    tree: String,
    /// The submitted cwd (relative to the root, already validated).
    cwd: String,
    /// The command, run inside the jail by `bash -lc`.
    cmd: String,
    timeout_s: u64,
    /// Host path of the results dir (`<checkout>/.gpu-jobs/<id>`) — the
    /// checkout COPY of the authoritative results (see `out_dir`).
    dir: String,
    /// Unix seconds.
    queued_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finished_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exit: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timed_out: Option<bool>,
    /// Why the results copy into the checkout was skipped (B1): a symlink or
    /// an unexpected existing file on the walk. None = copied fine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    copy_error: Option<String>,
}

impl Job {
    fn load(path: &Path) -> Option<Job> {
        serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
    }

    fn save(&self, dir: &Path) -> std::io::Result<()> {
        fs::write(
            dir.join(format!("{}.json", self.id)),
            serde_json::to_vec_pretty(self)?,
        )
    }

    fn path(&self, state: &Path, stage: &str) -> PathBuf {
        state.join(stage).join(format!("{}.json", self.id))
    }
}

// ---------------------------------------------------------------------------
// The queue
// ---------------------------------------------------------------------------

pub struct Queue {
    state: PathBuf,
    inner: Arc<(Mutex<bool>, Condvar)>,
}

impl Queue {
    /// The daemon's queue: real state dir, orphan recovery, one worker.
    pub fn start() -> anyhow::Result<Queue> {
        Queue::start_at(&state_dir()?)
    }

    /// Testable variant on an explicit state dir. Also used by bridge tests.
    pub fn start_at(state: &Path) -> anyhow::Result<Queue> {
        for stage in ["queued", "running", "done", "out"] {
            fs::create_dir_all(state.join(stage))?;
        }
        // the jail is written fresh on every start: the embedded copy is the
        // source of truth, and 0755 because the queue execs it directly
        let jail = state.join("gpujail.sh");
        fs::write(&jail, GPUJAIL)?;
        fs::set_permissions(&jail, fs::Permissions::from_mode(0o755))?;
        recover(state);
        let inner = Arc::new((Mutex::new(false), Condvar::new()));
        let worker = Worker {
            state: state.to_path_buf(),
            inner: inner.clone(),
        };
        std::thread::Builder::new()
            .name("gpu-queue".into())
            .spawn(move || worker.run())?;
        Ok(Queue {
            state: state.to_path_buf(),
            inner,
        })
    }

    fn wake(&self) {
        let (lock, cond) = &*self.inner;
        let mut pending = lock.lock().unwrap();
        *pending = true;
        cond.notify_one();
    }

    // --- the ops (called from the bridge dispatch) -------------------------

    /// `gpu.submit`: validate, persist under queued/, wake the worker.
    pub fn submit(&self, caller: &Caller, req: &Value) -> Result<Value, BridgeError> {
        let cwd = string_field(req, "cwd")?;
        let cmd = string_field(req, "cmd")?;
        if cmd.trim().is_empty() {
            return Err(BridgeError::new(
                "bad_request",
                "'cmd' must be a non-empty string",
            ));
        }
        if cmd.len() > 64 * 1024 {
            return Err(BridgeError::new(
                "bad_request",
                "'cmd' is too long (max 64 KiB)",
            ));
        }
        let timeout_s = match req.get("timeout_s") {
            None | Some(Value::Null) => DEFAULT_TIMEOUT_S,
            Some(Value::Number(n)) => n
                .as_u64()
                .filter(|t| (1..=MAX_TIMEOUT_S).contains(t))
                .ok_or_else(|| {
                    BridgeError::new(
                        "bad_request",
                        format!("'timeout_s' must be 1..={MAX_TIMEOUT_S} seconds"),
                    )
                })?,
            Some(_) => {
                return Err(BridgeError::new(
                    "bad_request",
                    "'timeout_s' must be a number",
                ))
            }
        };
        let root = caller_root(caller);
        let (checkout, tree) = validate_cwd(&root, &cwd, caller.forest.is_some())?;
        let id = new_id()
            .map_err(|e| BridgeError::new("failed", format!("generating a job id: {e}")))?;
        // the authoritative record of what ran: daemon-private, written here
        // and only COPIED into the checkout when the job finishes (B1)
        let out = out_dir(&self.state, &id);
        fs::create_dir_all(&out)
            .map_err(|e| BridgeError::new("failed", format!("creating {}: {e}", out.display())))?;
        fs::write(out.join("cmd"), &cmd)
            .map_err(|e| BridgeError::new("failed", format!("writing cmd: {e}")))?;
        // the checkout-side dir (and files/) is what the job writes into
        let dir = secure_results_dir(&checkout, &id)?;
        let job = Job {
            id: id.clone(),
            forest: root.display().to_string(),
            checkout: checkout.display().to_string(),
            tree: tree.clone(),
            cwd,
            cmd,
            timeout_s,
            dir: dir.display().to_string(),
            queued_at: unix_now(),
            started_at: None,
            finished_at: None,
            exit: None,
            timed_out: None,
            copy_error: None,
        };
        job.save(&self.state.join("queued"))
            .map_err(|e| BridgeError::new("failed", format!("persisting the job: {e}")))?;
        self.wake();
        Ok(json!({
            "id": id,
            "dir": box_dir(&tree, &id),
        }))
    }

    /// `gpu.status`: the job's state (and, when done, its exit and output
    /// tails). Only the caller's own jobs are visible.
    pub fn status(&self, caller: &Caller, req: &Value) -> Result<Value, BridgeError> {
        let id = string_field(req, "id")?;
        valid_id(&id)?;
        let (job, stage) = self
            .find_job(&id)
            .ok_or_else(|| BridgeError::new("failed", format!("no job '{id}'")))?;
        if job.forest != caller_root(caller).display().to_string() {
            // another forest's job is indistinguishable from a missing one
            return Err(BridgeError::new("failed", format!("no job '{id}'")));
        }
        Ok(self.status_value(&job, stage, true))
    }

    /// `gpu.ls`: every job of this caller's forest, newest last. Without
    /// tails — the client polls this; 64 KiB per job would be noise.
    pub fn ls(&self, caller: &Caller) -> Result<Value, BridgeError> {
        let root = caller_root(caller).display().to_string();
        let mut found: Vec<(Job, &'static str)> = Vec::new();
        for stage in ["done", "running", "queued"] {
            for entry in fs::read_dir(self.state.join(stage))
                .into_iter()
                .flatten()
                .flatten()
            {
                let Some(job) = Job::load(&entry.path()) else {
                    continue; // mid-write or corrupt: never fatal
                };
                if job.forest == root {
                    found.push((job, stage));
                }
            }
        }
        found.sort_by(|a, b| a.0.id.cmp(&b.0.id));
        let jobs: Vec<Value> = found
            .into_iter()
            .map(|(job, stage)| self.status_value(&job, stage, false))
            .collect();
        Ok(json!({ "jobs": jobs }))
    }

    // --- internals ----------------------------------------------------------

    /// The job and its stage, wherever it is.
    fn find_job(&self, id: &str) -> Option<(Job, &'static str)> {
        for stage in ["done", "running", "queued"] {
            let path = self.state.join(stage).join(format!("{id}.json"));
            if let Some(job) = Job::load(&path) {
                return Some((job, stage));
            }
        }
        None
    }

    /// The wire shape of a job's status (decisions §1). `tails` adds the
    /// stdout/stderr tails — read from the daemon-private results dir (B1),
    /// never from the box-writable checkout copy.
    fn status_value(&self, job: &Job, stage: &str, tails: bool) -> Value {
        let mut v = json!({
            "id": job.id,
            "state": stage,
            "dir": box_dir(&job.tree, &job.id),
        });
        if let Some(exit) = job.exit {
            v["exit"] = json!(exit);
        }
        if job.timed_out == Some(true) {
            v["timed_out"] = json!(true);
        }
        if stage == "queued" {
            v["queued_before"] = json!(self.queued_before(&job.id));
        }
        if stage == "done" {
            if tails {
                let out = out_dir(&self.state, &job.id);
                if let Some(t) = tail_file(out.join("stdout")) {
                    v["stdout_tail"] = json!(t);
                }
                if let Some(t) = tail_file(out.join("stderr")) {
                    v["stderr_tail"] = json!(t);
                }
            }
            // B1: a skipped copy (a symlink on the walk) is part of the result
            v["results_copied"] = json!(job.copy_error.is_none());
            if let Some(e) = &job.copy_error {
                v["results_error"] = json!(e);
            }
        }
        v
    }

    /// How many queued jobs (any forest — the queue is global FIFO) come
    /// before this one.
    fn queued_before(&self, id: &str) -> usize {
        fs::read_dir(self.state.join("queued"))
            .into_iter()
            .flatten()
            .flatten()
            // lossy + strip_suffix: never index a lossy string by the OsStr's
            // byte length — a non-UTF8 name in queued/ would slice mid-char
            // and panic the connection thread (found in review)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .strip_suffix(".json")
                    .is_some_and(|other| other.lt(id))
            })
            .count()
    }
}

struct Worker {
    state: PathBuf,
    inner: Arc<(Mutex<bool>, Condvar)>,
}

impl Worker {
    /// Claim-and-run forever: the one place jobs execute. One worker thread
    /// per daemon is the "one GPU job at a time" guarantee.
    fn run(&self) {
        loop {
            match claim_oldest(&self.state) {
                Some(mut job) => self.run_job(&mut job),
                None => {
                    let (lock, cond) = &*self.inner;
                    let mut pending = lock.lock().unwrap();
                    *pending = false; // the flag is a hint; the scan is the truth
                    let _ = cond
                        .wait_timeout(pending, std::time::Duration::from_millis(500))
                        .unwrap();
                }
            }
        }
    }

    /// Run one job: `timeout -k 5 <t> gpujail.sh <forest> -- bash -lc <cmd>`
    /// with GPU_JOB_DIR/GPU_JOB_FILES/BENTO_GPU_CWD in the wrapper env (the
    /// jail passes them to the job; argv only, no host shell). The daemon's
    /// own writes (stdout/stderr/exit/timed-out) go to its private
    /// `$state/out/<id>/` and are COPIED into the checkout at the end
    /// without ever following a symlink (B1). `exit` is written last —
    /// done == the exit file exists.
    fn run_job(&self, job: &mut Job) {
        eprintln!(
            "bento serve: gpu job {} running ({}s): bash -lc {:?}",
            job.id, job.timeout_s, job.cmd
        );
        job.started_at = Some(unix_now());
        let _ = job.save(&self.state.join("running")); // refresh started_at
        let out = out_dir(&self.state, &job.id);
        if let Err(e) = fs::create_dir_all(&out) {
            eprintln!("bento serve: gpu job {}: preparing out dir: {e}", job.id);
        }
        let jail = self.state.join("gpujail.sh");
        let status = Command::new("timeout")
            .arg("-k")
            .arg("5")
            .arg(format!("{}s", job.timeout_s))
            .arg(&jail)
            .arg(&job.forest)
            .arg("--")
            .arg("bash")
            .arg("-lc")
            .arg(&job.cmd)
            // M1: the job never reads the daemon's stdin and never shares its
            // session — no tty to TIOCSTI into (gpujail.sh adds --new-session)
            .stdin(Stdio::null())
            .env("BENTO_GPU_CWD", &job.cwd)
            .env("GPU_JOB_DIR", &job.dir)
            .env("GPU_JOB_FILES", Path::new(&job.dir).join("files"))
            // the daemon's own env decides the display mode (gpujail.sh):
            // BENTO_GPU_REAL_DISPLAY=1 opts into the user's real display
            .current_dir(&job.forest)
            .stdout(Stdio::from(exclusive_or_sink(&out.join("stdout"))))
            .stderr(Stdio::from(exclusive_or_sink(&out.join("stderr"))))
            .status();
        let exit = match status {
            Ok(s) => s.code().unwrap_or(253),
            Err(e) => {
                // gpujail itself could not start; the job never ran
                let _ = fs::write(
                    out.join("stderr"),
                    format!("bento serve: starting the gpu jail: {e}"),
                );
                254
            }
        };
        let timed_out = exit == 124; // timeout's code; the marker disambiguates
        if timed_out {
            let _ = fs::write(
                out.join("timed-out"),
                format!("killed after {}s\n", job.timeout_s),
            );
        }
        // `exit` in the state dir: the daemon's own record of "it is over"
        let _ = fs::write(out.join("exit"), format!("{exit}\n"));
        // the checkout copy: `exit` lands last (the box's done signal), and
        // only when everything else landed (B1: never follow, record skips)
        job.copy_error = copy_results(
            Path::new(&job.checkout),
            &job.id,
            &out,
            timed_out,
        )
        .err();
        if let Some(e) = &job.copy_error {
            eprintln!("bento serve: gpu job {}: results not copied: {e}", job.id);
        }
        job.exit = Some(exit);
        job.timed_out = Some(timed_out);
        job.finished_at = Some(unix_now());
        let running = job.path(&self.state, "running");
        let _ = job.save(&self.state.join("done"));
        let _ = fs::remove_file(running);
        eprintln!("bento serve: gpu job {} done, exit {exit}", job.id);
    }
}

/// Stdout/stderr sink in the daemon-private out dir: fresh (O_EXCL) or
/// /dev/null when even that fails (nothing better to do with output).
fn exclusive_or_sink(path: &Path) -> File {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .unwrap_or_else(|_| File::create("/dev/null").expect("/dev/null"))
}

/// Copy the authoritative results (`out`) into the checkout's
/// `.gpu-jobs/<id>/` — never following a symlink (B1). Every hop
/// (`checkout`, `.gpu-jobs`, `<id>`) is opened with O_DIRECTORY|O_NOFOLLOW
/// relative to the previous fd, files are created with
/// O_CREAT|O_EXCL|O_NOFOLLOW. The first refusal (a symlink, or a file that
/// already exists) skips the REST of the copy — in particular `exit`, the
/// box's done signal, is only written when everything else landed. The
/// caller records the error in the job record; `gpu.status` shows it.
fn copy_results(checkout: &Path, id: &str, out: &Path, timed_out: bool) -> Result<(), String> {
    let refuse = |what: &str, e: std::io::Error| {
        format!("results not copied: {what}: {e} (symlink planted? never followed)")
    };
    let fd_checkout = sys::open_dir_at(None, checkout.as_os_str())
        .map_err(|e| refuse(&format!("opening {}", checkout.display()), e))?;
    let fd_gj = match sys::open_dir_at(Some(&fd_checkout), OsStr::new(".gpu-jobs")) {
        Ok(fd) => fd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            sys::mkdir_at(&fd_checkout, OsStr::new(".gpu-jobs"))
                .map_err(|e| refuse("creating .gpu-jobs", e))?
        }
        Err(e) => return Err(refuse("opening .gpu-jobs", e)),
    };
    // the job dir should exist (submit made it); recreate if the box or the
    // job removed it — mkdirat never follows the final component
    let fd_id = match sys::open_dir_at(Some(&fd_gj), OsStr::new(id)) {
        Ok(fd) => fd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            sys::mkdir_at(&fd_gj, OsStr::new(id)).map_err(|e| refuse("creating the job dir", e))?
        }
        Err(e) => return Err(refuse("opening the job dir", e)),
    };
    let mut names: Vec<&str> = vec!["cmd", "stdout", "stderr"];
    if timed_out {
        names.push("timed-out");
    }
    for name in names {
        let Ok(bytes) = fs::read(out.join(name)) else {
            continue; // nothing there (e.g. no cmd on a recovered old job)
        };
        sys::create_exclusive_at(&fd_id, name, &bytes)
            .map_err(|e| refuse(&format!("creating {name}"), e))?;
    }
    let exit = fs::read_to_string(out.join("exit")).unwrap_or_default();
    sys::create_exclusive_at(&fd_id, "exit", exit.as_bytes())
        .map_err(|e| refuse("creating exit", e))?;
    Ok(())
}

/// Claim the oldest queued job by renaming it to running/ — the rename is
/// atomic, so two claimers can never take the same job.
fn claim_oldest(state: &Path) -> Option<Job> {
    let mut ids: Vec<String> = fs::read_dir(state.join("queued"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json").map(String::from)
        })
        .collect();
    ids.sort();
    for id in ids {
        let from = state.join("queued").join(format!("{id}.json"));
        let to = state.join("running").join(format!("{id}.json"));
        if fs::rename(&from, &to).is_ok() {
            if let Some(job) = Job::load(&to) {
                return Some(job);
            }
            // unreadable record: drop it, try the next
            let _ = fs::remove_file(&to);
        }
    }
    None
}

/// Startup recovery (decisions §6): a job found in running/ belonged to a
/// daemon that died; mark it done with exit 250 so its agent is not stuck.
/// All daemon writes go to the daemon-private out dir; the checkout copy
/// happens through the same symlink-refusing walk as a live job (B1).
fn recover(state: &Path) {
    let running = state.join("running");
    for entry in fs::read_dir(&running).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(mut job) = Job::load(&path) else {
            let _ = fs::remove_file(&path); // corrupt beyond repair
            continue;
        };
        let out = out_dir(state, &job.id);
        let _ = fs::create_dir_all(&out);
        let stderr = out.join("stderr");
        if !stderr.exists() {
            let _ = fs::write(&stderr, "bento serve: the queue died while this job ran\n");
        }
        let exit_file = out.join("exit");
        if !exit_file.exists() {
            let _ = fs::write(&exit_file, format!("{RUNNER_DIED}\n"));
        }
        job.copy_error = copy_results(
            Path::new(&job.checkout),
            &job.id,
            &out,
            job.timed_out == Some(true),
        )
        .err();
        job.exit = Some(RUNNER_DIED);
        job.finished_at = Some(unix_now());
        let _ = job.save(&state.join("done"));
        let _ = fs::remove_file(&path);
        eprintln!(
            "bento serve: gpu job {} recovered as exit {RUNNER_DIED}",
            job.id
        );
    }
    // backstop from R5: scratch homes leaked by a SIGKILLed wrapper
    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .saturating_sub(24 * 3600);
    for entry in fs::read_dir("/tmp").into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("gpujail-home.")
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() < cutoff)
                .unwrap_or(false)
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// Validation (decisions §1 safety rules for gpu.*)
// ---------------------------------------------------------------------------

/// The caller's root: the workforest dir, or the single checkout dir.
fn caller_root(caller: &Caller) -> PathBuf {
    caller.path.clone()
}

/// Resolve a submitted cwd against the caller's root and find the checkout
/// that will hold the results. `workforest` switches the rules:
///
/// - workforest: the first component names the checkout (`<root>/<tree>/…`);
///   it must be a dir with a `.git` (forest.rs's own definition of a tree).
/// - single checkout: the root IS the checkout; the cwd is relative to it.
///
/// Either way the realpath of the joined dir must stay inside the realpath of
/// the checkout — symlinks cannot point the job (or its results) elsewhere.
fn validate_cwd(
    root: &Path,
    cwd: &str,
    workforest: bool,
) -> Result<(PathBuf, String), BridgeError> {
    if cwd.is_empty() {
        return Err(BridgeError::new("bad_request", "'cwd' must not be empty"));
    }
    let rel = Path::new(cwd);
    if rel.is_absolute() {
        return Err(BridgeError::new(
            "bad_request",
            format!("'cwd' must be relative to the forest, got '{cwd}'"),
        ));
    }
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(BridgeError::new(
            "bad_request",
            format!("'cwd' must be a plain relative path without '.' or '..' (got '{cwd}')"),
        ));
    }
    let (checkout, rel_to_checkout) = if workforest {
        let tree = rel
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .ok_or_else(|| BridgeError::new("bad_request", "'cwd' must name a checkout"))?;
        let checkout = root.join(tree);
        if !checkout.join(".git").exists() {
            return Err(BridgeError::new(
                "bad_request",
                format!("'{tree}' is not a checkout of this forest (no .git)"),
            ));
        }
        let rest: PathBuf = rel.components().skip(1).collect();
        (checkout, rest)
    } else {
        (root.to_path_buf(), rel.to_path_buf())
    };
    // the dir itself must exist and stay inside the checkout after realpath
    let full = checkout.join(&rel_to_checkout);
    let real = fs::canonicalize(&full).map_err(|_| {
        BridgeError::new(
            "bad_request",
            format!("cwd '{}' not found ({})", cwd, full.display()),
        )
    })?;
    let real_checkout = fs::canonicalize(&checkout).map_err(|_| {
        BridgeError::new(
            "bad_request",
            format!("checkout '{}' not found", checkout.display()),
        )
    })?;
    if !real.starts_with(&real_checkout) {
        return Err(BridgeError::new(
            "bad_request",
            format!("cwd '{cwd}' escapes its checkout (symlink?)"),
        ));
    }
    let tree = forest_tree_name(&real_checkout);
    Ok((real_checkout, tree))
}

/// The checkout's name as the box mounts it: `/workspace/<tree>` — slug of
/// the dir's basename, exactly like forest.rs names workforest trees.
fn forest_tree_name(checkout: &Path) -> String {
    let base = checkout
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    crate::forest::slug(&base)
}

/// Create `<checkout>/.gpu-jobs/<id>/files/` safely (B1): every hop is
/// opened or created relative to the previous fd with O_NOFOLLOW — a
/// symlinked `.gpu-jobs` or job dir (or anything planted in between) is
/// refused, never followed. `.gpu-jobs/.gitignore` (`*`) keeps `git status`
/// clean without touching any git config.
fn secure_results_dir(checkout: &Path, id: &str) -> Result<PathBuf, BridgeError> {
    let fd_checkout = sys::open_dir_at(None, checkout.as_os_str())
        .map_err(|e| BridgeError::new("bad_request", format!("{}: {e}", checkout.display())))?;
    let fd_gj = match sys::open_dir_at(Some(&fd_checkout), OsStr::new(".gpu-jobs")) {
        Ok(fd) => fd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            sys::mkdir_at(&fd_checkout, OsStr::new(".gpu-jobs")).map_err(|e| {
                BridgeError::new("failed", format!("creating .gpu-jobs: {e}"))
            })?
        }
        Err(e) => {
            return Err(BridgeError::new(
                "bad_request",
                format!(".gpu-jobs is not a real directory (symlink?): {e}"),
            ))
        }
    };
    // best effort: EEXIST (however it got there) is fine, it is gitignore
    let _ = sys::create_exclusive_at(&fd_gj, ".gitignore", b"*\n");
    let fd_id = sys::mkdir_at(&fd_gj, OsStr::new(id)).map_err(|e| {
        BridgeError::new("failed", format!("creating the job dir {id}: {e}"))
    })?;
    sys::mkdir_at(&fd_id, OsStr::new("files")).map_err(|e| {
        BridgeError::new("failed", format!("creating the job's files dir: {e}"))
    })?;
    Ok(checkout.join(".gpu-jobs").join(id))
}

// ---------------------------------------------------------------------------
// Ids, time, tails
// ---------------------------------------------------------------------------

/// `<YYYYMMDD-HHMMSS>-<4 hex>` (decisions §6): local-time prefixed so ids
/// sort by submission order across all forests.
fn new_id() -> anyhow::Result<String> {
    id_at(SystemTime::now(), random_hex()?)
}

fn id_at(t: SystemTime, suffix: String) -> anyhow::Result<String> {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("clock before the epoch: {e}"))?
        .as_secs() as i64
        + local_offset_secs();
    let days = secs.div_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let tod = secs.rem_euclid(86400);
    Ok(format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}-{suffix}",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    ))
}

/// Local timezone offset for ids: read from `date` (the daemon's host has it;
/// no tz database in std). UTC on any failure — an id that sorts by UTC is
/// still unique and FIFO.
fn local_offset_secs() -> i64 {
    let out = Command::new("date").arg("+%z").output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // +0100 / -0500
            if s.len() == 5 && (s.starts_with('+') || s.starts_with('-')) {
                let sign = if s.starts_with('+') { 1 } else { -1 };
                let h: i64 = s[1..3].parse().unwrap_or(0);
                let m: i64 = s[3..5].parse().unwrap_or(0);
                return sign * (h * 3600 + m * 60);
            }
            0
        }
        _ => 0,
    }
}

/// Howard Hinnant's civil-from-days: days since 1970-01-01 -> (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 4 hex chars from /dev/urandom.
fn random_hex() -> anyhow::Result<String> {
    let mut bytes = [0u8; 2];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Job ids have exactly this shape — `gpu.status` checks before touching the
/// filesystem with a caller-supplied string.
fn valid_id(id: &str) -> Result<(), BridgeError> {
    let bytes = id.as_bytes();
    let ok = bytes.len() == 20 // YYYYMMDD-HHMMSS-hhhh
        && bytes[..8].iter().all(|b| b.is_ascii_digit())
        && bytes[8] == b'-'
        && bytes[9..15].iter().all(|b| b.is_ascii_digit())
        && bytes[15] == b'-'
        && bytes[16..]
            .iter()
            .all(|b| b.is_ascii_hexdigit() && (b.is_ascii_digit() || b.is_ascii_lowercase()));
    if ok {
        Ok(())
    } else {
        Err(BridgeError::new(
            "bad_request",
            format!("bad job id '{id}'"),
        ))
    }
}

/// The results dir as the box sees it (decisions §1).
fn box_dir(tree: &str, id: &str) -> String {
    format!("/workspace/{tree}/.gpu-jobs/{id}")
}

/// The last `TAIL` bytes of a file, lossy UTF-8; None when unreadable.
/// Never follows a symlink (B1): results are tailed from the daemon-private
/// out dir, and anything planted where a box could reach is skipped.
fn tail_file(path: PathBuf) -> Option<String> {
    let mut file = sys::open_read_nofollow(&path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > TAIL {
        file.seek(std::io::SeekFrom::Start(len - TAIL)).ok()?;
    }
    let mut buf = Vec::new();
    file.take(TAIL).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bento-gpu-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A scratch workforest: root/{a,b}/ with .git markers, a/outside/ symlink.
    /// `name` keeps parallel tests out of each other's tempdirs.
    fn workforest(name: &str) -> (PathBuf, PathBuf) {
        let root = tempdir(name);
        let forests = root.join("forests").join("wf");
        let repos = root.join("repos");
        fs::create_dir_all(&repos).unwrap();
        for tree in ["a", "b"] {
            let dir = forests.join(tree).join("sub");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("f"), "x").unwrap();
            fs::write(dir.join(".git_marker"), "").unwrap();
            // .git must be a dir entry; a file is enough for `.exists()`
            fs::write(dir.parent().unwrap().join(".git"), "gitdir: fake").unwrap();
        }
        fs::create_dir_all(root.join("outside")).unwrap();
        let link = forests.join("a").join("link");
        std::os::unix::fs::symlink(root.join("outside"), &link).unwrap();
        (forests, root)
    }

    #[test]
    fn ids_have_the_contracted_shape_and_order() {
        let t = |secs| UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let utc_midnight_2026_11_13: u64 = 1_794_528_000; // date -u -d "2026-11-13" +%s
        let id = id_at(t(utc_midnight_2026_11_13), "ab12".into()).unwrap();
        assert_eq!(id, "20261113-000000-ab12");
        assert!(valid_id(&id).is_ok());
        // civil_from_days sanity: known dates (day numbers via date +%s/86400)
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_722), (2023, 12, 31));
        assert_eq!(civil_from_days(20_636), (2026, 7, 2));
        // a bad id is rejected before any filesystem use
        for bad in [
            "",
            "x",
            "../escape",
            "20261113-000000",
            "20261113-000000-AB12",
            "20261113-000000-ab1g",
        ] {
            assert!(valid_id(bad).is_err(), "{bad}");
        }
        // same second, different suffix: distinct, equal order
        let a = id_at(t(1000), "aa".into()).unwrap();
        let b = id_at(t(1000), "bb".into()).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn local_offset_is_a_plausible_utc_offset() {
        let off = local_offset_secs();
        assert!(off.abs() <= 14 * 3600, "{off}");
        assert!(off % 60 == 0, "{off}");
    }

    #[test]
    fn cwd_validation_accepts_checkouts_and_rejects_escapes() {
        let (forest, root) = workforest("cwd");
        // workforest rules: first component must be a checkout
        assert!(validate_cwd(&forest, "a", true).is_ok());
        assert!(validate_cwd(&forest, "a/sub", true).is_ok());
        // nonexistent dirs are rejected (realpath fails)
        assert!(validate_cwd(&forest, "a/nope", true).is_err());
        for bad in [
            "",
            "/a",
            "a/..",
            "..",
            "../outside",
            "c",
            "a/../b",
            ".",
            "./a",
            // symlink escape: a/link -> outside the forest
            "a/link",
        ] {
            let err = validate_cwd(&forest, bad, true);
            assert!(err.is_err(), "workforest cwd '{bad}' must be rejected");
        }
        // single-checkout rules: the root itself is the checkout
        let solo = forest.join("a");
        assert!(validate_cwd(&solo, "sub", false).is_ok());
        assert!(validate_cwd(&solo, "sub/../sub", false).is_err());
        assert!(validate_cwd(&solo, "..", false).is_err());
        assert!(validate_cwd(&solo, "link", false).is_err()); // symlink out
        let _ = root; // scratch root lives under this tempdir
    }

    #[test]
    fn results_dirs_are_created_once_and_never_through_symlinks() {
        let (forest, _) = workforest("results1");
        let checkout = fs::canonicalize(forest.join("a")).unwrap();
        let dir = secure_results_dir(&checkout, "20261113-000000-aaaa").unwrap();
        assert!(dir.join("files").is_dir());
        assert_eq!(
            fs::read_to_string(checkout.join(".gpu-jobs/.gitignore")).unwrap(),
            "*\n"
        );
        // an existing job dir is refused (also covers a symlink planted there)
        assert!(secure_results_dir(&checkout, "20261113-000000-aaaa").is_err());
        // a symlinked .gpu-jobs is refused
        let (forest2, _root2) = workforest("results2");
        let checkout2 = fs::canonicalize(forest2.join("b")).unwrap();
        std::os::unix::fs::symlink(checkout.join(".gpu-jobs"), checkout2.join(".gpu-jobs"))
            .unwrap();
        let err = secure_results_dir(&checkout2, "20261113-000000-bbbb").unwrap_err();
        assert_eq!(err.code, "bad_request");
        // a file where .gpu-jobs should be is refused too
        let (forest3, _root3) = workforest("results3");
        let checkout3 = fs::canonicalize(forest3.join("b")).unwrap();
        fs::write(checkout3.join(".gpu-jobs"), "x").unwrap();
        assert!(secure_results_dir(&checkout3, "20261113-000000-cccc").is_err());
    }

    #[test]
    fn claims_are_oldest_first_and_by_rename() {
        let state = tempdir("claim");
        for stage in ["queued", "running", "done"] {
            fs::create_dir_all(state.join(stage)).unwrap();
        }
        let job = |id: &str| Job {
            id: id.into(),
            forest: "/tmp/f".into(),
            checkout: "/tmp/f/a".into(),
            tree: "a".into(),
            cwd: "a".into(),
            cmd: "true".into(),
            timeout_s: 10,
            dir: "/tmp/f/a/.gpu-jobs".into(),
            queued_at: 1,
            started_at: None,
            finished_at: None,
            exit: None,
            timed_out: None,
            copy_error: None,
        };
        job("20261113-000001-0001")
            .save(&state.join("queued"))
            .unwrap();
        job("20261113-000003-0003")
            .save(&state.join("queued"))
            .unwrap();
        job("20261113-000002-0002")
            .save(&state.join("queued"))
            .unwrap();
        assert_eq!(claim_oldest(&state).unwrap().id, "20261113-000001-0001");
        assert_eq!(claim_oldest(&state).unwrap().id, "20261113-000002-0002");
        assert_eq!(claim_oldest(&state).unwrap().id, "20261113-000003-0003");
        assert!(claim_oldest(&state).is_none());
        // the claimed records now live in running/
        assert_eq!(fs::read_dir(state.join("running")).unwrap().count(), 3);
        assert_eq!(fs::read_dir(state.join("queued")).unwrap().count(), 0);
    }

    #[test]
    fn recovery_marks_orphans_exit_250() {
        let state = tempdir("recover");
        for stage in ["queued", "running", "done"] {
            fs::create_dir_all(state.join(stage)).unwrap();
        }
        // a job the "previous daemon" left in running/, with its results dir
        let results = state.join("results");
        fs::create_dir_all(&results).unwrap();
        Job {
            id: "20261113-120000-00ff".into(),
            forest: "/tmp/f".into(),
            checkout: "/tmp/f/a".into(),
            tree: "a".into(),
            cwd: "a".into(),
            cmd: "sleep 1000".into(),
            timeout_s: 600,
            dir: results.display().to_string(),
            queued_at: 1,
            started_at: Some(2),
            finished_at: None,
            exit: None,
            timed_out: None,
            copy_error: None,
        }
        .save(&state.join("running"))
        .unwrap();
        recover(&state);
        assert_eq!(fs::read_dir(state.join("running")).unwrap().count(), 0);
        let recovered = Job::load(&state.join("done/20261113-120000-00ff.json")).unwrap();
        assert_eq!(recovered.exit, Some(RUNNER_DIED));
        // the authoritative exit lands in the daemon's out dir (B1); the
        // checkout "/tmp/f/a" does not exist, so the copy is refused and
        // recorded rather than best-effort
        assert_eq!(
            fs::read_to_string(state.join("out").join("20261113-120000-00ff").join("exit"))
                .unwrap()
                .trim(),
            "250"
        );
        assert!(recovered.copy_error.is_some());
        // a queued job survives recovery untouched
    }

    // --- B1: the daemon never follows box-controlled symlinks --------------

    /// A canary file OUTSIDE the forest: the box and jobs must never be able
    /// to make the daemon write here (or read it back).
    fn canary_at(root: &Path) -> PathBuf {
        let canary = root.join("outside").join("canary");
        fs::write(&canary, "PRECIOUS\n").unwrap();
        canary
    }

    #[test]
    fn recovery_never_follows_result_symlinks() {
        let (forest, root) = workforest("recover-symlink");
        let state = tempdir("recover-symlink-state");
        for stage in ["queued", "running", "done"] {
            fs::create_dir_all(state.join(stage)).unwrap();
        }
        let canary = canary_at(&root);
        // the "previous daemon" died running this job; the box/job has since
        // replaced `exit` and `stderr` with symlinks to the canary
        let checkout = forest.join("a");
        let dir = checkout.join(".gpu-jobs").join("20261113-120000-00ee");
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&canary, dir.join("exit")).unwrap();
        std::os::unix::fs::symlink(&canary, dir.join("stderr")).unwrap();
        Job {
            id: "20261113-120000-00ee".into(),
            forest: forest.display().to_string(),
            checkout: checkout.display().to_string(),
            tree: "a".into(),
            cwd: "a".into(),
            cmd: "sleep 1000".into(),
            timeout_s: 600,
            dir: dir.display().to_string(),
            queued_at: 1,
            started_at: Some(2),
            finished_at: None,
            exit: None,
            timed_out: None,
            copy_error: None,
        }
        .save(&state.join("running"))
        .unwrap();
        recover(&state);
        // the canary is untouched: the daemon never followed the symlinks
        assert_eq!(
            fs::read_to_string(&canary).unwrap(),
            "PRECIOUS\n",
            "recover() followed a symlink and wrote the canary"
        );
        // the authoritative results live in the daemon's own state dir
        let out = state.join("out").join("20261113-120000-00ee");
        assert_eq!(fs::read_to_string(out.join("exit")).unwrap().trim(), "250");
        // the refused copy into the checkout is recorded in the done record
        let done_raw: serde_json::Value = serde_json::from_str(&fs::read_to_string(
            state.join("done").join("20261113-120000-00ee.json"),
        )
        .unwrap())
        .unwrap();
        assert!(
            done_raw["copy_error"].is_string(),
            "the refused copy must be recorded: {done_raw}"
        );
    }

    #[test]
    fn status_tails_never_follow_symlinks() {
        let (forest, root) = workforest("tail-symlink");
        let state = tempdir("tail-symlink-state");
        for stage in ["queued", "running", "done"] {
            fs::create_dir_all(state.join(stage)).unwrap();
        }
        // a "secret" outside the forest; the box swapped the done job's
        // stdout for a symlink to it
        let secret = root.join("outside").join("secret");
        fs::write(&secret, "TOPSECRET-contents\n").unwrap();
        let checkout = forest.join("a");
        let dir = checkout.join(".gpu-jobs").join("20261113-130000-00aa");
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&secret, dir.join("stdout")).unwrap();
        Job {
            id: "20261113-130000-00aa".into(),
            forest: forest.display().to_string(),
            checkout: checkout.display().to_string(),
            tree: "a".into(),
            cwd: "a".into(),
            cmd: "cat the secret".into(),
            timeout_s: 10,
            dir: dir.display().to_string(),
            queued_at: 1,
            started_at: Some(2),
            finished_at: Some(3),
            exit: Some(0),
            timed_out: Some(false),
            copy_error: None,
        }
        .save(&state.join("done"))
        .unwrap();
        let queue = Queue {
            state: state.clone(),
            inner: Arc::new((Mutex::new(false), Condvar::new())),
        };
        let rt: tokio::runtime::Handle =
            Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()))
                .handle()
                .clone();
        let caller = Caller {
            box_name: "bento-forest_wf".into(),
            path: forest.clone(),
            forest: Some("wf".into()),
            machine: "wf".into(),
            rt,
        };
        let st = queue
            .status(&caller, &json!({"op":"gpu.status","id":"20261113-130000-00aa"}))
            .unwrap();
        let text = serde_json::to_string(&st).unwrap();
        assert!(
            !text.contains("TOPSECRET"),
            "a symlinked stdout must not be read back: {text}"
        );
    }

    /// End to end through the real jail: the JOB itself plants symlinks
    /// (`exit`, `timed-out`) over its results and exits 124, so the daemon
    /// writes those files right after. Neither write may follow.
    #[test]
    fn a_running_job_cannot_redirect_daemon_result_writes() {
        let (forest, root) = workforest("e2e-symlink");
        let state = tempdir("e2e-symlink-state");
        let queue = Queue::start_at(&state).unwrap();
        let canary = canary_at(&root);
        let rt: tokio::runtime::Handle =
            Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()))
                .handle()
                .clone();
        let caller = Caller {
            box_name: "bento-forest_wf".into(),
            path: forest.clone(),
            forest: Some("wf".into()),
            machine: "wf".into(),
            rt,
        };
        let cmd = format!(
            r#"ln -sf '{}' "$GPU_JOB_DIR/exit"; ln -sf '{}' "$GPU_JOB_DIR/timed-out"; exit 124"#,
            canary.display(),
            canary.display()
        );
        let reply = queue
            .submit(
                &caller,
                &json!({"op":"gpu.submit","cwd":"a","cmd":cmd,"timeout_s":60}),
            )
            .unwrap();
        let id = reply["id"].as_str().unwrap().to_string();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let status = loop {
            let st = queue
                .status(&caller, &json!({"op":"gpu.status","id":id}))
                .unwrap();
            if st["state"] == "done" {
                break st;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job never finished: {st}"
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        assert_eq!(status["exit"], json!(124), "{status}");
        assert_eq!(status["timed_out"], json!(true), "{status}");
        // the canary was never written through the symlinks...
        assert_eq!(
            fs::read_to_string(&canary).unwrap(),
            "PRECIOUS\n",
            "the daemon followed a job-planted symlink"
        );
        // ...nor read back through one
        let text = serde_json::to_string(&status).unwrap();
        assert!(!text.contains("PRECIOUS"), "{text}");
        // the authoritative results live in the daemon's state dir
        let out = state.join("out").join(&id);
        assert_eq!(fs::read_to_string(out.join("exit")).unwrap().trim(), "124");
        assert!(out.join("timed-out").exists(), "marker in the state dir");
        // and the refused copy into the checkout is recorded in the status
        assert_eq!(status["results_copied"], json!(false), "{status}");
        assert!(status["results_error"].is_string(), "{status}");
    }

    /// The copy walk itself: happy path, symlinked dirs refused, planted
    /// files refused and `exit` then withheld.
    #[test]
    fn result_copies_walk_dirs_without_following() {
        let (forest, root) = workforest("copy-walk");
        let checkout = forest.join("a");
        let canary = canary_at(&root);
        // the authoritative out dir, as the daemon would have it
        let out = root.join("out-authoritative");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("cmd"), "the cmd\n").unwrap();
        fs::write(out.join("stdout"), "the output\n").unwrap();
        fs::write(out.join("exit"), "0\n").unwrap();

        // happy path: everything lands, exit last
        copy_results(&checkout, "20261113-140000-0001", &out, false).unwrap();
        let done = checkout.join(".gpu-jobs").join("20261113-140000-0001");
        assert_eq!(fs::read_to_string(done.join("stdout")).unwrap(), "the output\n");
        assert_eq!(fs::read_to_string(done.join("cmd")).unwrap(), "the cmd\n");
        assert_eq!(fs::read_to_string(done.join("exit")).unwrap(), "0\n");
        assert!(!done.join("timed-out").exists());

        // a planted `stdout` symlink: refused, exit withheld, canary
        // untouched (cmd was already copied — files before the refusal stay)
        let dir2 = checkout.join(".gpu-jobs").join("20261113-140000-0002");
        fs::create_dir_all(&dir2).unwrap();
        std::os::unix::fs::symlink(&canary, dir2.join("stdout")).unwrap();
        let err = copy_results(&checkout, "20261113-140000-0002", &out, false).unwrap_err();
        assert!(err.contains("stdout"), "{err}");
        assert!(!dir2.join("exit").exists(), "exit must be withheld");
        assert!(!dir2.join("stderr").exists(), "nothing after the refusal");
        assert_eq!(fs::read_to_string(&canary).unwrap(), "PRECIOUS\n");

        // a symlinked job dir: refused outright
        let dir3 = checkout.join(".gpu-jobs");
        std::os::unix::fs::symlink(&out, dir3.join("20261113-140000-0003")).unwrap();
        assert!(copy_results(&checkout, "20261113-140000-0003", &out, false).is_err());

        // a symlinked .gpu-jobs: refused outright
        let checkout_b = forest.join("b");
        std::os::unix::fs::symlink(checkout.join(".gpu-jobs"), checkout_b.join(".gpu-jobs")).unwrap();
        assert!(copy_results(&checkout_b, "20261113-140000-0004", &out, false).is_err());

        // timed-out marker is copied when the job timed out
        fs::write(out.join("timed-out"), "killed after 5s\n").unwrap();
        copy_results(&checkout, "20261113-140000-0005", &out, true).unwrap();
        assert!(checkout
            .join(".gpu-jobs")
            .join("20261113-140000-0005")
            .join("timed-out")
            .exists());
    }

    /// M1: a job gets no tty and no controlling tty, even when the daemon
    /// itself runs on one. Run this test under a pty to mean anything:
    ///   script -qefc 'cargo test jobs_get_no_tty' /dev/null
    #[test]
    fn jobs_get_no_tty_and_no_controlling_tty() {
        let (forest, _root) = workforest("m1-tty");
        let state = tempdir("m1-tty-state");
        let queue = Queue::start_at(&state).unwrap();
        let rt: tokio::runtime::Handle =
            Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()))
                .handle()
                .clone();
        let caller = Caller {
            box_name: "bento-forest_wf".into(),
            path: forest.clone(),
            forest: Some("wf".into()),
            machine: "wf".into(),
            rt,
        };
        let cmd =
            r#"head -c 4; tty; (exec 3<>/dev/tty) 2>/dev/null && echo HAS-CTTY || echo NO-CTTY"#;
        let reply = queue
            .submit(
                &caller,
                &json!({"op":"gpu.submit","cwd":"a","cmd":cmd,"timeout_s":60}),
            )
            .unwrap();
        let id = reply["id"].as_str().unwrap().to_string();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let status = loop {
            let st = queue
                .status(&caller, &json!({"op":"gpu.status","id":id}))
                .unwrap();
            if st["state"] == "done" {
                break st;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job never finished: {st}"
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        assert_eq!(status["exit"], json!(0), "{status}");
        let stdout = fs::read_to_string(
            state.join("out").join(&id).join("stdout"),
        )
        .unwrap_or_default();
        assert!(
            !stdout.contains("SECR"),
            "the job must not read the daemon's stdin (Stdio::null): {stdout:?}"
        );
        assert!(
            stdout.contains("not a tty"),
            "stdin must be null, job saw: {stdout:?}"
        );
        assert!(
            stdout.contains("NO-CTTY"),
            "the job must have no controlling tty (bwrap --new-session): {stdout:?}"
        );
    }

    #[test]
    fn queued_before_survives_corrupt_names() {
        let state = tempdir("qb");
        for stage in ["queued", "running", "done"] {
            fs::create_dir_all(state.join(stage)).unwrap();
        }
        let queue = Queue {
            state: state.clone(),
            inner: Arc::new((Mutex::new(false), Condvar::new())),
        };
        // a corrupt non-UTF8 name in queued/ (torn state dir) must not panic
        use std::os::unix::ffi::OsStringExt as _;
        let bad = state
            .join("queued")
            .join(std::ffi::OsString::from_vec(b"\xffx.json".to_vec()));
        fs::write(&bad, "{}").unwrap();
        // a well-formed earlier job still counts; the bad name (replacement
        // char sorts after digits) does not
        let job = |id: &str| Job {
            id: id.into(),
            forest: "/tmp/f".into(),
            checkout: "/tmp/f/a".into(),
            tree: "a".into(),
            cwd: "a".into(),
            cmd: "true".into(),
            timeout_s: 10,
            dir: "/tmp/f/a/.gpu-jobs".into(),
            queued_at: 1,
            started_at: None,
            finished_at: None,
            exit: None,
            timed_out: None,
            copy_error: None,
        };
        job("20261113-000100-0000")
            .save(&state.join("queued"))
            .unwrap();
        assert_eq!(queue.queued_before("20261113-235959-zzzz"), 1);
        let _ = fs::remove_file(&bad);
    }

    #[test]
    fn the_queue_serves_the_ops_on_a_temp_state_dir() {
        let (forest, _root) = workforest("ops-forest");
        let state = tempdir("ops-state");
        let queue = Queue::start_at(&state).unwrap();
        // jail was written executable
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(state.join("gpujail.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(fs::read_to_string(state.join("gpujail.sh"))
            .unwrap()
            .starts_with("#!/usr/bin/env bash"));

        let rt: tokio::runtime::Handle =
            Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()))
                .handle()
                .clone();
        let caller = Caller {
            box_name: "bento-forest_wf".into(),
            path: forest.clone(),
            forest: Some("wf".into()),
            machine: "wf".into(),
            rt,
        };
        // validation happens before anything is queued
        for req in [
            json!({"op":"gpu.submit","cwd":"a","cmd":""}),
            json!({"op":"gpu.submit","cwd":"../x","cmd":"true"}),
            json!({"op":"gpu.submit","cwd":"c","cmd":"true"}),
            json!({"op":"gpu.submit","cwd":"a","cmd":"true","timeout_s":0}),
            json!({"op":"gpu.submit","cwd":"a","cmd":"true","timeout_s":99999}),
            json!({"op":"gpu.submit","cmd":"true"}),
        ] {
            let err = queue.submit(&caller, &req).unwrap_err();
            assert_eq!(err.code, "bad_request", "{req}: {}", err.message);
        }
        assert_eq!(fs::read_dir(state.join("queued")).unwrap().count(), 0);
        // a valid submit queues and answers with the box-visible dir
        let reply = queue
            .submit(
                &caller,
                &json!({"op":"gpu.submit","cwd":"a","cmd":"echo hi","timeout_s":30}),
            )
            .unwrap();
        let id = reply["id"].as_str().unwrap().to_string();
        assert!(valid_id(&id).is_ok());
        assert_eq!(
            reply["dir"].as_str().unwrap(),
            format!("/workspace/a/.gpu-jobs/{id}")
        );
        // the worker will claim it, fail to run it (no real forest at /tmp/f
        // — well, it IS real: the jail runs `true` in bwrap and finishes) —
        // either way it must reach done with an integer exit
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let status = loop {
            let st = queue
                .status(&caller, &json!({"op":"gpu.status","id":id}))
                .unwrap();
            if st["state"] == "done" {
                break st;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job never finished: {st}"
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        assert!(status["exit"].is_i64(), "{status}");
        assert_eq!(
            status["exit"],
            json!(0),
            "bash -lc 'echo hi' must succeed: {status}"
        );
        let done = Job::load(&state.join("done").join(format!("{id}.json"))).unwrap();
        let out = fs::read_to_string(Path::new(&done.dir).join("stdout")).unwrap();
        assert_eq!(out.trim(), "hi");
        // the exit file exists (written last) and the cmd file records the command
        assert!(Path::new(&done.dir).join("exit").exists());
        assert_eq!(
            fs::read_to_string(Path::new(&done.dir).join("cmd")).unwrap(),
            "echo hi"
        );
        // status is caller-scoped: another forest's caller sees nothing
        let rt2: tokio::runtime::Handle =
            Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()))
                .handle()
                .clone();
        let stranger = Caller {
            box_name: "bento-forest_other".into(),
            path: tempdir("other"),
            forest: None,
            machine: "other".into(),
            rt: rt2,
        };
        assert!(queue
            .status(&stranger, &json!({"op":"gpu.status","id":id}))
            .is_err());
        let jobs = queue.ls(&stranger).unwrap();
        assert_eq!(jobs["jobs"].as_array().unwrap().len(), 0);
        // and ls shows it to its own forest
        let jobs = queue.ls(&caller).unwrap();
        assert_eq!(jobs["jobs"].as_array().unwrap().len(), 1);
    }
}
