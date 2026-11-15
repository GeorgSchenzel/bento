//! tools/bento-run.c, built with the host's C compiler and run on the host:
//! exit codes, forwarded signals, and what happens to the command when the
//! wrapper itself is SIGKILLed (as msb's agent does when bento's connection
//! drops). Skipped when there is no `cc`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn build() -> Option<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/bento-run.c");
    let out = std::env::temp_dir().join(format!("bento-run-test-{}", std::process::id()));
    let ok = Command::new("cc")
        .args(["-O2", "-Wall", "-Werror", "-DBENTO_RUN_GRACE_MS=1000", "-o"])
        .arg(&out)
        .arg(&src)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("no working cc: skipping bento-run tests");
        return None;
    }
    Some(out)
}

fn tmpfile(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("bento-run-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn alive(pid: i32) -> bool {
    // a zombie counts as gone
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => !s
            .rsplit(')')
            .next()
            .unwrap_or("")
            .trim_start()
            .starts_with('Z'),
        Err(_) => false,
    }
}

fn wait_for(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_pid(path: &Path) -> i32 {
    wait_for("a pid file", Duration::from_secs(5), || {
        std::fs::read_to_string(path).is_ok_and(|s| s.trim().parse::<i32>().is_ok())
    });
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn bento_run() {
    let Some(run) = build() else { return };
    let code = |args: &[&str]| {
        Command::new(&run).arg("/")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
            .status
            .code()
            .unwrap()
    };

    // exit codes
    assert_eq!(code(&["sh", "-c", "exit 3"]), 3);
    assert_eq!(code(&["true"]), 0);
    assert_eq!(code(&["sh", "-c", "kill -TERM $$"]), 143);
    assert_eq!(code(&["sh", "-c", "kill -KILL $$"]), 137);
    assert_eq!(code(&["bento-no-such-command"]), 127);
    let noexec = tmpfile("noexec");
    std::fs::write(&noexec, "#!/bin/sh\n").unwrap();
    assert_eq!(code(&[noexec.to_str().unwrap()]), 126);
    assert_eq!(code(&[]), 125);

    // the dir: bento-run changes to it itself; a missing one is 125
    let mut nodir = Command::new(&run);
    nodir.args(["/no/such/dir", "true"]).stdin(Stdio::null());
    assert_eq!(nodir.output().unwrap().status.code(), Some(125));
    let mut pwd = Command::new(&run);
    let out = pwd.args(["/tmp", "pwd"]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "/tmp");
    let mut nocmd = Command::new(&run);
    assert_eq!(nocmd.arg("/").stdin(Stdio::null()).output().unwrap().status.code(), Some(125));

    // stdout and stderr pass through byte-exact
    let out = Command::new(&run).arg("/")
        .args(["sh", "-c", "printf 'a\\0b'; printf e >&2"])
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"a\0b");
    assert_eq!(out.stderr, b"e");

    // a forwarded signal reaches the command, whose exit code comes back
    let marker = tmpfile("fwd");
    let mut child = Command::new(&run).arg("/")
        .args([
            "sh",
            "-c",
            &format!(
                "trap 'echo got > {}; exit 7' TERM; echo $$ > {}.pid; while :; do sleep 0.05; done",
                marker.display(),
                marker.display()
            ),
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    read_pid(&PathBuf::from(format!("{}.pid", marker.display())));
    unsafe { libc_kill(child.id() as i32, 15) };
    assert_eq!(child.wait().unwrap().code(), Some(7));
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "got");

    // the wrapper SIGKILLed (the caller dropped): SIGHUP reaches the command
    let hup = tmpfile("hup");
    let pidf = tmpfile("hup-pid");
    let mut child = Command::new(&run).arg("/")
        .args([
            "sh",
            "-c",
            &format!(
                "trap 'echo hup > {}; exit 0' HUP; echo $$ > {}; while :; do sleep 0.05; done",
                hup.display(),
                pidf.display()
            ),
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pid = read_pid(&pidf);
    child.kill().unwrap();
    child.wait().unwrap();
    wait_for("SIGHUP", Duration::from_secs(3), || hup.exists());
    wait_for("the command to end", Duration::from_secs(3), || !alive(pid));

    // ... and SIGKILL after the grace period when it ignores SIGHUP
    let pidf = tmpfile("kill-pid");
    let mut child = Command::new(&run).arg("/")
        .args([
            "sh",
            "-c",
            &format!(
                "trap '' HUP; echo $$ > {}; while :; do sleep 0.05; done",
                pidf.display()
            ),
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pid = read_pid(&pidf);
    let killed_at = Instant::now();
    child.kill().unwrap();
    child.wait().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(alive(pid), "killed before the grace period");
    wait_for("SIGKILL", Duration::from_secs(4), || !alive(pid));
    assert!(killed_at.elapsed() >= Duration::from_millis(900));

    // processes a command leaves behind survive its normal end
    let bgf = tmpfile("bg-pid");
    let status = Command::new(&run).arg("/")
        .args([
            "sh",
            "-c",
            &format!("sleep 30 & echo $! > {}", bgf.display()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let bg = read_pid(&bgf);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(alive(bg), "a background process died with its command");
    unsafe { libc_kill(bg, 9) };

    for f in [noexec, marker, hup, bgf] {
        let _ = std::fs::remove_file(f);
    }
    let _ = std::fs::remove_file(run);
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
