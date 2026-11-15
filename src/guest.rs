//! What runs inside a box: bento's paths, its env, and the boot script that
//! `create`, `start` and `restart` run before anything else (contracts.md,
//! Inside a box). Pure: everything here is a string, tested on the host.

use std::path::Path;

use crate::config::Config;
use crate::util::sh_quote;

/// The embedded tools' GC root (builder.rs builds it).
pub const TOOLS: &str = "/nix/var/nix/gcroots/bento/tools";
/// The config's tools, one GC root each.
pub const EXTRA: &str = "/nix/var/nix/gcroots/bento/extra";
/// The command wrapper every `exec` runs through (tools/bento-run.c).
pub const RUN: &str = "/nix/var/nix/gcroots/bento/tools/bin/bento-run";
pub const BASH: &str = "/nix/var/nix/gcroots/bento/tools/bin/bash";

/// The data disk, as msb mounts it; its subdirs are bound at boot.
pub const DATA: &str = "/bento/data";
/// `files` entries, as msb mounts them (read-only); bound at their guest
/// paths at boot.
pub const FILES: &str = "/bento/files";
/// Git token files (read-only), one per host.
pub const GIT: &str = "/bento/git";
/// Channel sockets.
pub const CHANNELS: &str = "/run/bento";
/// bento's own sockets, logs and pids in the box.
pub const SYS: &str = "/run/bento-sys";
pub const NIX_SOCK: &str = "/run/bento-sys/nix.sock";
pub const GITCONFIG: &str = "/etc/bento/gitconfig";
pub const WORKSPACE: &str = "/workspace";
pub const DOCKER_ROOT: &str = "/var/lib/docker";

/// Path of a `files` entry's msb mount.
pub fn file_mount(i: usize) -> String {
    format!("{FILES}/{i}")
}

pub fn git_mount(i: usize) -> String {
    format!("{GIT}/{i}")
}

/// bento's own env, set in every process it starts (the names are
/// names::RESERVED_ENV, plus BENTO_BOX).
pub fn own_env(config: &Config, box_name: &str, home: &Path) -> Vec<(String, String)> {
    let mut path = vec![format!("{TOOLS}/bin")];
    for (tool, _) in &config.tools {
        path.push(format!("{EXTRA}/{tool}/bin"));
    }
    path.extend(
        [
            "/nix/var/nix/profiles/default/bin",
            "/usr/local/sbin",
            "/usr/local/bin",
            "/usr/sbin",
            "/usr/bin",
            "/sbin",
            "/bin",
        ]
        .map(String::from),
    );
    let certs = format!("{TOOLS}/etc/ssl/certs/ca-bundle.crt");
    vec![
        ("PATH".into(), path.join(":")),
        ("HOME".into(), home.display().to_string()),
        ("NIX_REMOTE".into(), format!("unix://{NIX_SOCK}")),
        (
            "NIX_CONFIG".into(),
            "experimental-features = nix-command flakes".into(),
        ),
        ("NIX_SSL_CERT_FILE".into(), certs.clone()),
        ("SSL_CERT_FILE".into(), certs.clone()),
        ("GIT_SSL_CAINFO".into(), certs),
        ("GIT_CONFIG_SYSTEM".into(), GITCONFIG.into()),
        // claude allows skipping permissions as root only in a sandbox
        ("IS_SANDBOX".into(), "1".into()),
        ("TERMINFO_DIRS".into(), format!("{TOOLS}/share/terminfo")),
        ("BENTO_BOX".into(), box_name.into()),
    ]
}

/// Merge env layers, later ones winning, keeping the first position of
/// each name.
pub fn merge_env(layers: &[&[(String, String)]]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for layer in layers {
        for (k, v) in layer.iter() {
            match out.iter_mut().find(|(name, _)| name == k) {
                Some(slot) => slot.1 = v.clone(),
                None => out.push((k.clone(), v.clone())),
            }
        }
    }
    out
}

/// The username a git host expects with a token.
pub fn git_user(host: &str) -> &'static str {
    if host == "github.com" {
        "x-access-token"
    } else {
        "oauth2"
    }
}

/// A value in a git config file: quoted, with `\` and `"` escaped and
/// newlines dropped.
fn git_value(v: &str) -> String {
    let clean: String = v.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    format!("\"{}\"", clean.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The box's system git config: a credential helper per configured host,
/// the host's identity if it has one.
pub fn gitconfig(config: &Config, identity: Option<(&str, &str)>) -> String {
    let mut out = String::new();
    for (i, (host, _)) in config.git.iter().enumerate() {
        out.push_str(&format!(
            "[credential \"https://{host}\"]\n\thelper =\n\thelper = {}\n",
            git_value(&format!("/etc/bento/git-credential-{i}"))
        ));
    }
    if let Some((name, email)) = identity {
        out.push_str(&format!(
            "[user]\n\tname = {}\n\temail = {}\n",
            git_value(name),
            git_value(email)
        ));
    }
    out
}

/// A credential helper script: answers `get` with the token file's content.
pub fn git_helper(i: usize, host: &str) -> String {
    format!(
        "#!{BASH}\n[ \"$1\" = get ] || exit 0\nprintf 'username=%s\\npassword=%s\\n' {} \"$(cat {})\"\n",
        git_user(host),
        git_mount(i)
    )
}

/// What the boot script needs beyond the config.
pub struct BootInput<'a> {
    pub config: &'a Config,
    pub home: &'a Path,
    /// For each `files` entry: whether its host side is a dir.
    pub files_are_dirs: Vec<bool>,
    pub identity: Option<(String, String)>,
}

/// Runs as root at every boot, before anything else: binds the data disk's
/// dirs and the read-only files, writes git's config, starts the channel
/// shims, nix forwarding and dockerd, and returns once each answers.
pub fn boot_script(input: &BootInput) -> String {
    let c = input.config;
    let home = input.home.display().to_string();
    let q = |s: &str| sh_quote(s);
    let mut s = String::new();
    s.push_str(&format!(
        r#"set -eu
S={SYS}
mkdir -p "$S" {CHANNELS} /etc/bento
ismnt() {{ findmnt -rn --mountpoint "$1" >/dev/null 2>&1; }}
fail() {{ echo "bento: $1" >&2; [ -n "${{2:-}}" ] && tail -n 20 "$2" >&2; exit 1; }}
ismnt {DATA} || fail "the data disk is not mounted at {DATA}"
mkdir -p {DATA}/workspace {DATA}/home {DATA}/docker
bindto() {{ mkdir -p "$2"; ismnt "$2" || mount --bind "$1" "$2"; }}
bindto {DATA}/workspace {WORKSPACE}
bindto {DATA}/home {home}
bindto {DATA}/docker {DOCKER_ROOT}
rofile() {{
  if [ "$3" = dir ]; then mkdir -p "$2"; else mkdir -p "$(dirname "$2")"; [ -e "$2" ] || : > "$2"; fi
  ismnt "$2" || {{ mount --bind "$1" "$2" && mount -o remount,bind,ro "$2"; }} || fail "can't mount $2"
}}
"#,
        home = q(&home),
    ));
    for (i, entry) in c.files.iter().enumerate() {
        let kind = if input.files_are_dirs.get(i).copied().unwrap_or(false) {
            "dir"
        } else {
            "file"
        };
        s.push_str(&format!(
            "rofile {} {} {kind}\n",
            file_mount(i),
            q(&entry.guest)
        ));
    }
    let identity = input
        .identity
        .as_ref()
        .map(|(n, e)| (n.as_str(), e.as_str()));
    s.push_str(&format!(
        "printf %s {} > {GITCONFIG}\n",
        q(&gitconfig(c, identity))
    ));
    s.push_str("rm -f /etc/bento/git-credential-*\n");
    for (i, (host, _)) in c.git.iter().enumerate() {
        let path = format!("/etc/bento/git-credential-{i}");
        s.push_str(&format!(
            "printf %s {} > {path}\nchmod 755 {path}\n",
            q(&git_helper(i, host))
        ));
    }
    // channels: the route survives a stop, the shim and its socket don't
    // (a stale socket file stays: remove it first). The shim's settings work
    // around msb's vsock bugs A and B (phase0.md).
    for (ch, port) in c.channel_ports() {
        let sock = format!("{CHANNELS}/{}.sock", ch.name);
        s.push_str(&format!(
            "rm -f {sock}\nsetsid -f socat -b 32768 UNIX-LISTEN:{sock},fork,mode=666 \
             VSOCK-CONNECT:2:{port},setsockopt=40:2:x0000000100000000,setsockopt=40:0:x0000000100000000 \
             </dev/null >\"$S/channel-{name}.log\" 2>&1\n",
            name = ch.name
        ));
    }
    // /run and /var/run are plain rootfs dirs (msb mounts no tmpfs; /run
    // holds msb's own files), so dockerd's pidfile and socket in /var/run
    // outlive a stop: on the next boot the stale pid belongs to another
    // process and dockerd refuses to start. A tmpfs on /var/run (docker's
    // only) starts it empty at every boot.
    s.push_str(&format!(
        r#"rm -f {NIX_SOCK}
setsid -f socat UNIX-LISTEN:{NIX_SOCK},fork TCP:host.microsandbox.internal:{port} </dev/null >"$S/nix-fwd.log" 2>&1
ismnt /var/run || mount -t tmpfs -o mode=755 tmpfs /var/run
setsid -f dockerd --data-root {DOCKER_ROOT} </dev/null >"$S/dockerd.log" 2>&1
"#,
        port = c.instance.nix_port
    ));
    for (ch, _) in c.channel_ports() {
        let sock = format!("{CHANNELS}/{}.sock", ch.name);
        s.push_str(&format!(
            "i=0; until [ -S {sock} ]; do i=$((i+1)); [ $i -gt 100 ] && fail \"channel {name} didn't start\" \"$S/channel-{name}.log\"; sleep 0.1; done\n",
            name = ch.name
        ));
    }
    s.push_str(
        r#"i=0; until timeout 10 nix store info >/dev/null 2>"$S/nix-check.log"; do i=$((i+1)); [ $i -gt 50 ] && fail "the builder's nix daemon doesn't answer" "$S/nix-check.log"; sleep 0.2; done
i=0; until docker info >/dev/null 2>&1; do i=$((i+1)); [ $i -gt 150 ] && fail "dockerd didn't start" "$S/dockerd.log"; sleep 0.2; done
"#,
    );
    s
}

/// Copies a `copy` entry in: replaces the guest path with the archive's
/// single top-level entry (named `$2` in it), extracted into `$1`.
pub const COPY_IN: &str = r#"set -eu
mkdir -p -- "$1"
rm -rf -- "$1/$2"
tar -x --no-same-owner -C "$1" -f -"#;

/// `bento cp` into the box: extract the archive into an existing dir.
pub const EXTRACT: &str = r#"set -eu
tar -x --no-same-owner -C "$1" -f -"#;

/// What a guest path is: `dir`, `file` (anything else that exists, links
/// included), `missing` (its parent is a dir) or `noparent`.
pub const STAT: &str = r#"if [ -d "$1" ] && [ ! -L "$1" ]; then echo dir
elif [ -e "$1" ] || [ -L "$1" ]; then echo file
elif [ -d "$(dirname -- "$1")" ]; then echo missing
else echo noparent; fi"#;

/// Archive `$2` (a name in dir `$1`, or `.`) to stdout.
pub const ARCHIVE: &str = r#"set -eu
tar -c -C "$1" -f - -- "$2""#;

/// Signals every process but PID 1 and itself: HUP, then KILL after 5 s.
/// Run (by bash, it forks nothing while it looks) before a stop, so commands
/// see SIGHUP first (contracts.md, stop). Kernel threads and zombies have an
/// empty cmdline and don't count.
pub const HANGUP_ALL: &str = r#"busy() {
  local p c
  for p in /proc/[0-9]*; do
    p=${p#/proc/}
    [[ $p == 1 || $p == $$ ]] && continue
    c=
    read -r -d '' c < "/proc/$p/cmdline" 2>/dev/null
    [[ -n $c ]] && return 0
  done
  return 1
}
kill -HUP -1 2>/dev/null
kill -CONT -1 2>/dev/null
i=0
while (( i < 50 )); do
  busy || exit 0
  sleep 0.1
  i=$((i+1))
done
kill -KILL -1 2>/dev/null
exit 0"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Entry};
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn cfg() -> Config {
        let inst = crate::instance::Instance::resolve(None, Some(OsStr::new("/home/u"))).unwrap();
        config::parse(
            Some(
                r#"
channels: {wok: {socket: /w.sock}, stew: {socket: /s.sock}}
tools: {claude: github:a/b/c#claude, hc: github:a/hc/d#hc}
git: {github.com: /t1, gitlab.com: /t2}
"#,
            ),
            &inst,
        )
        .unwrap()
    }

    #[test]
    fn path_has_bentos_tools_then_the_configs() {
        let env = own_env(&cfg(), "b", Path::new("/home/u"));
        let path = &env.iter().find(|(k, _)| k == "PATH").unwrap().1;
        assert!(path.starts_with(&format!("{TOOLS}/bin:{EXTRA}/claude/bin:{EXTRA}/hc/bin:")));
        for (k, _) in &env {
            assert!(crate::names::is_reserved_env(k), "{k} must be reserved");
        }
    }

    #[test]
    fn env_layers_later_wins() {
        let a = vec![("A".to_string(), "1".to_string()), ("B".into(), "1".into())];
        let b = vec![("B".to_string(), "2".to_string()), ("C".into(), "2".into())];
        let c = vec![("A".to_string(), "3".to_string())];
        assert_eq!(
            merge_env(&[&a, &b, &c]),
            vec![
                ("A".to_string(), "3".to_string()),
                ("B".into(), "2".into()),
                ("C".into(), "2".into())
            ]
        );
    }

    #[test]
    fn gitconfig_quotes_identity() {
        let g = gitconfig(&cfg(), Some(("A \"b\" \\c\n[core]", "a@b")));
        assert!(g.contains("[credential \"https://github.com\"]"));
        assert!(g.contains("helper = \"/etc/bento/git-credential-1\""));
        assert!(g.contains("name = \"A \\\"b\\\" \\\\c[core]\""));
        assert_eq!(g.matches("[core]").count(), 1); // inside a quoted value only
        assert!(!g.contains("\n[core]"));
        let h = git_helper(0, "github.com");
        assert!(h.contains("x-access-token") && h.contains("/bento/git/0"));
        assert!(git_helper(1, "gitlab.com").contains("oauth2"));
    }

    #[test]
    fn git_helper_runs() {
        let dir = std::env::temp_dir().join(format!("bento-gh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tok = dir.join("tok");
        std::fs::write(&tok, "secret-1\n").unwrap();
        let script = git_helper(0, "github.com")
            .replace(&format!("#!{BASH}\n"), "")
            .replace("/bento/git/0", tok.to_str().unwrap());
        let run = |arg: &str| {
            let out = std::process::Command::new("sh")
                .args(["-c", &script, "sh", arg])
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(run("get"), "username=x-access-token\npassword=secret-1\n");
        assert_eq!(run("store"), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn boot_script_quotes_paths_and_starts_everything() {
        let mut c = cfg();
        c.files.push(Entry {
            guest: "/home/u/it's $(x)".into(),
            host: PathBuf::from("/h"),
        });
        let s = boot_script(&BootInput {
            config: &c,
            home: Path::new("/home/u"),
            files_are_dirs: vec![true],
            identity: Some(("n".into(), "e".into())),
        });
        assert!(s.contains("rofile /bento/files/0 '/home/u/it'\\''s $(x)' dir\n"));
        assert!(s.contains("VSOCK-CONNECT:2:6000,")); // stew, first by name
        assert!(s.contains("UNIX-LISTEN:/run/bento/stew.sock,fork,mode=666"));
        assert!(s.contains("UNIX-LISTEN:/run/bento/wok.sock"));
        assert!(s.contains("VSOCK-CONNECT:2:6001,"));
        assert!(s.contains("TCP:host.microsandbox.internal:17760"));
        assert!(s.contains("-b 32768"));
        assert!(s.contains("dockerd --data-root /var/lib/docker"));
        assert!(s.contains("docker info"));
        // dockerd's pidfile never outlives a boot: a fresh /var/run first
        let run = s.find("mount -t tmpfs -o mode=755 tmpfs /var/run").unwrap();
        assert!(run < s.find("setsid -f dockerd").unwrap());
        // binds happen before files (which may lie in HOME) and before services
        let home = s.find("bindto /bento/data/home '/home/u'").unwrap();
        let files = s.find("rofile /bento/files/0").unwrap();
        let shim = s.find("setsid -f socat").unwrap();
        assert!(home < files && files < shim);
        // the whole script is valid sh
        let out = std::process::Command::new("sh")
            .args(["-n", "-c", &s])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn sh(script: &str, args: &[&str]) -> std::process::Output {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", script, "sh"]).args(args);
        cmd.output().unwrap()
    }

    #[test]
    fn stat_script() {
        let dir = std::env::temp_dir().join(format!("bento-stat-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("d")).unwrap();
        std::fs::write(dir.join("f"), "").unwrap();
        std::os::unix::fs::symlink("/nonexistent", dir.join("l")).unwrap_or(());
        std::os::unix::fs::symlink(dir.join("d"), dir.join("ld")).unwrap_or(());
        let st = |p: &str| {
            String::from_utf8(sh(STAT, &[dir.join(p).to_str().unwrap()]).stdout)
                .unwrap()
                .trim()
                .to_string()
        };
        assert_eq!(st("d"), "dir");
        assert_eq!(st("f"), "file");
        assert_eq!(st("l"), "file");
        assert_eq!(st("ld"), "file");
        assert_eq!(st("m"), "missing");
        assert_eq!(st("m/x"), "noparent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn helper_scripts_parse() {
        for s in [COPY_IN, EXTRACT, STAT, ARCHIVE, HANGUP_ALL] {
            let out = std::process::Command::new("bash")
                .args(["-n", "-c", s])
                .output()
                .unwrap();
            assert!(out.status.success(), "{s}");
        }
    }
}
