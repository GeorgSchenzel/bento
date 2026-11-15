//! Running things in a box: bento's own scripts (collected, or streaming an
//! archive in or out) and the caller's commands (`exec`, `enter`), byte-exact,
//! with stdin, a pty, signals and exit codes as contracts.md says.

use std::io::{Read, Write};

use anyhow::{anyhow, bail, Context as _, Result};
use microsandbox::protocol::exec::ExecFailureKind;
use microsandbox::sandbox::Sandbox;
use microsandbox::{ExecEvent, MicrosandboxError};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;

use crate::fail;
use crate::guest::{BASH, RUN};

pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    /// Fail unless the script exited 0, with its stderr as the message.
    pub fn check(self, what: &str) -> Result<Output> {
        if self.code != 0 {
            let err = String::from_utf8_lossy(&self.stderr);
            bail!("{what}: exit {}: {}", self.code, err.trim());
        }
        Ok(self)
    }
}

/// Run `bash -c <script> bash <args…>` as root in `/`, collecting its output.
pub async fn script(
    sb: &Sandbox,
    script: &str,
    args: &[&str],
    env: &[(String, String)],
) -> Result<Output> {
    let mut argv = vec!["-c".to_string(), script.to_string(), "bash".to_string()];
    argv.extend(args.iter().map(|a| a.to_string()));
    let env = env.to_vec();
    let out = sb
        .exec_with(BASH, move |o| o.args(argv).envs(env).cwd("/"))
        .await
        .context("running a script in the box")?;
    Ok(Output {
        code: out.status().code,
        stdout: out.stdout_bytes().to_vec(),
        stderr: out.stderr_bytes().to_vec(),
    })
}

/// A Write that sends what it gets to an async receiver.
struct ChanWriter(mpsc::Sender<Vec<u8>>);

impl Write for ChanWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = buf.len().min(64 * 1024);
        self.0
            .blocking_send(buf[..n].to_vec())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "box side closed"))?;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A Read fed by an async sender.
struct ChanReader {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChanReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.blocking_recv() {
                Some(chunk) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Run a script with stdin fed by `produce` (on a blocking thread).
pub async fn script_with_input<F>(
    sb: &Sandbox,
    script: &str,
    args: &[&str],
    env: &[(String, String)],
    produce: F,
) -> Result<Output>
where
    F: FnOnce(&mut dyn Write) -> Result<()> + Send + 'static,
{
    let mut argv = vec!["-c".to_string(), script.to_string(), "bash".to_string()];
    argv.extend(args.iter().map(|a| a.to_string()));
    let env = env.to_vec();
    let mut handle = sb
        .exec_stream_with(BASH, move |o| o.args(argv).envs(env).cwd("/").stdin_pipe())
        .await
        .context("running a script in the box")?;
    let sink = handle.take_stdin().context("no stdin for the script")?;
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
    let producer = tokio::task::spawn_blocking(move || {
        let mut w = ChanWriter(tx);
        produce(&mut w)
    });
    let feeder = async {
        while let Some(chunk) = rx.recv().await {
            sink.write(&chunk).await.context("writing to the box")?;
        }
        sink.close().await.context("closing the box's stdin")?;
        Ok::<_, anyhow::Error>(())
    };
    let collect = async {
        let (mut out, mut err) = (vec![], vec![]);
        while let Some(ev) = handle.recv().await {
            match ev {
                ExecEvent::Stdout(b) => out.extend_from_slice(&b),
                ExecEvent::Stderr(b) => err.extend_from_slice(&b),
                ExecEvent::Exited { code } => return Ok((code, out, err)),
                ExecEvent::Failed(f) => return Err(anyhow!("{}", f.message)),
                _ => {}
            }
        }
        Err(anyhow!("lost the connection to the box"))
    };
    let (fed, collected) = tokio::join!(feeder, collect);
    let produced = producer.await.context("archive thread")?;
    let (code, stdout, stderr) = collected?;
    if code == 0 {
        fed?;
        produced?;
    }
    Ok(Output {
        code,
        stdout,
        stderr,
    })
}

/// Run a script whose stdout `consume` reads (on a blocking thread).
/// Returns the exit code and stderr.
pub async fn script_with_output<F>(
    sb: &Sandbox,
    script: &str,
    args: &[&str],
    env: &[(String, String)],
    consume: F,
) -> Result<(i32, Vec<u8>)>
where
    F: FnOnce(&mut dyn Read) -> Result<()> + Send + 'static,
{
    let mut argv = vec!["-c".to_string(), script.to_string(), "bash".to_string()];
    argv.extend(args.iter().map(|a| a.to_string()));
    let env = env.to_vec();
    let mut handle = sb
        .exec_stream_with(BASH, move |o| o.args(argv).envs(env).cwd("/"))
        .await
        .context("running a script in the box")?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
    let consumer = tokio::task::spawn_blocking(move || {
        let mut r = ChanReader {
            rx,
            buf: vec![],
            pos: 0,
        };
        let res = consume(&mut r);
        // drain, so the box side never blocks on a reader that gave up
        let mut sink = [0u8; 8192];
        while r.read(&mut sink).is_ok_and(|n| n > 0) {}
        res
    });
    let mut err = vec![];
    let mut code = None;
    while let Some(ev) = handle.recv().await {
        match ev {
            ExecEvent::Stdout(b) => {
                if tx.send(b.to_vec()).await.is_err() {
                    break;
                }
            }
            ExecEvent::Stderr(b) => err.extend_from_slice(&b),
            ExecEvent::Exited { code: c } => {
                code = Some(c);
                break;
            }
            ExecEvent::Failed(f) => bail!("{}", f.message),
            _ => {}
        }
    }
    drop(tx);
    let consumed = consumer.await.context("archive thread")?;
    let code = code.ok_or_else(|| anyhow!("lost the connection to the box"))?;
    if code == 0 {
        consumed?;
    }
    Ok((code, err))
}

/// A caller's command.
pub struct Command {
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub stdin: bool,
    pub tty: bool,
}

/// The exit code `bento exec` returns for the command's code as msb
/// reports it: negative is a signal.
pub fn exit_code(code: i32) -> u8 {
    match code {
        c if c < 0 => (128 + (-c).min(127)) as u8,
        c if c <= 255 => c as u8,
        _ => fail::OTHER,
    }
}

/// The exit code for a command that couldn't start.
pub fn failed_code(kind: ExecFailureKind) -> u8 {
    match kind {
        ExecFailureKind::NotFound => 127,
        ExecFailureKind::PermissionDenied | ExecFailureKind::NotExecutable => 126,
        _ => fail::OTHER,
    }
}

/// Run a caller's command through bento-run; returns its exit code.
pub async fn run(sb: &Sandbox, cmd: Command) -> Result<u8> {
    // bento-run changes to the dir itself: msb would fail the spawn with
    // ENOENT (127) for a missing one, and the contract says 125
    let mut argv = vec![cmd.cwd.clone()];
    argv.extend(cmd.argv.iter().cloned());
    let env = cmd.env.clone();
    let (stdin, tty) = (cmd.stdin, cmd.tty);
    let mut handle = sb
        .exec_stream_with(RUN, move |o| {
            let o = o
                .args(std::mem::take(&mut argv))
                .envs(env)
                .cwd("/")
                .tty(tty);
            if stdin {
                o.stdin_pipe()
            } else {
                o.stdin_null()
            }
        })
        .await
        .map_err(|e| match e {
            MicrosandboxError::ExecFailed(f) => fail::fail(failed_code(f.kind), f.message.clone()),
            e => anyhow::Error::from(e).context("running the command"),
        })?;
    let control = handle.control();

    let _raw = if tty { RawMode::enter() } else { None };
    if tty {
        if let Some((rows, cols)) = window_size() {
            let _ = control.resize(rows, cols).await;
        }
    }

    // stdin: a blocking reader thread, fed to the box with flow control
    let feeder = if stdin {
        let sink = handle.take_stdin().context("no stdin")?;
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
        std::thread::spawn(move || {
            let mut input = std::io::stdin().lock();
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match input.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
        Some(tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                if sink.write(&chunk).await.is_err() {
                    return;
                }
            }
            let _ = sink.close().await;
        }))
    } else {
        None
    };

    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigwinch = signal(SignalKind::window_change())?;

    let mut out = tokio::io::stdout();
    let mut err = tokio::io::stderr();
    let (mut out_ok, mut err_ok) = (true, true);
    let code = loop {
        tokio::select! {
            ev = handle.recv() => match ev {
                Some(ExecEvent::Stdout(b)) => {
                    if out_ok {
                        out_ok = out.write_all(&b).await.is_ok() && out.flush().await.is_ok();
                    }
                }
                Some(ExecEvent::Stderr(b)) => {
                    if err_ok {
                        err_ok = err.write_all(&b).await.is_ok() && err.flush().await.is_ok();
                    }
                }
                Some(ExecEvent::Exited { code }) => break exit_code(code),
                Some(ExecEvent::Failed(f)) => {
                    drop(_raw);
                    eprintln!("bento: {}", f.message);
                    break failed_code(f.kind);
                }
                Some(_) => {}
                // the box went away under the command
                None => break 128 + 9,
            },
            _ = sigint.recv() => { let _ = control.signal(libc::SIGINT).await; }
            _ = sigterm.recv() => { let _ = control.signal(libc::SIGTERM).await; }
            _ = sighup.recv() => { let _ = control.signal(libc::SIGHUP).await; }
            _ = sigwinch.recv(), if tty => {
                if let Some((rows, cols)) = window_size() {
                    let _ = control.resize(rows, cols).await;
                }
            }
        }
    };
    if let Some(f) = feeder {
        f.abort();
    }
    Ok(code)
}

fn window_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    for fd in [1, 0, 2] {
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_row > 0 {
            return Some((ws.ws_row, ws.ws_col));
        }
    }
    None
}

/// The host terminal in raw mode until dropped (the box's pty does the
/// line discipline).
struct RawMode {
    saved: libc::termios,
}

impl RawMode {
    fn enter() -> Option<RawMode> {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
            return None;
        }
        let saved = t;
        unsafe { libc::cfmakeraw(&mut t) };
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) } != 0 {
            return None;
        }
        Some(RawMode { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.saved) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(3), 3);
        assert_eq!(exit_code(255), 255);
        assert_eq!(exit_code(-9), 137);
        assert_eq!(exit_code(-1), 129);
        assert_eq!(exit_code(143), 143);
        assert_eq!(exit_code(300), fail::OTHER);
        assert_eq!(failed_code(ExecFailureKind::NotFound), 127);
        assert_eq!(failed_code(ExecFailureKind::PermissionDenied), 126);
        assert_eq!(failed_code(ExecFailureKind::BadCwd), fail::OTHER);
    }

    #[test]
    fn chan_reader_and_writer() {
        let rt = tokio::runtime::Builder::new_multi_thread().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = mpsc::channel(2);
            let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
            let expect = data.clone();
            let w = tokio::task::spawn_blocking(move || {
                let mut w = ChanWriter(tx);
                w.write_all(&data).unwrap();
            });
            let r = tokio::task::spawn_blocking(move || {
                let mut r = ChanReader {
                    rx,
                    buf: vec![],
                    pos: 0,
                };
                let mut got = vec![];
                r.read_to_end(&mut got).unwrap();
                got
            });
            w.await.unwrap();
            assert_eq!(r.await.unwrap(), expect);
        });
    }
}
