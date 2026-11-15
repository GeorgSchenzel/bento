//! The builder: one sandbox per instance, the only writer of the instance's
//! store. It runs nix-daemon behind a socat listener published on host
//! `127.0.0.1:<nix_port>` (boxes reach only host 127.0.0.1, A1); boxes mount
//! the store read-only and reach the daemon as host.microsandbox.internal.
//! It is never restarted (published ports and background processes don't
//! survive stop→start): when unhealthy or outdated it is recreated.
//!
//! The config's tools are built through `exec` (never through the embedded
//! flake or the builder's spec), so adding one neither changes the spec nor
//! recreates the builder.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use microsandbox::sandbox::{Sandbox, SandboxHandle};
use microsandbox::MicrosandboxError;

use crate::config::Config;
use crate::guest::{EXTRA, TOOLS};
use crate::instance::Instance;
use crate::state::Lock;
use crate::{names, store, util};

const FLAKE: &str = include_str!("../tools/flake.nix");
const FLAKE_LOCK: &str = include_str!("../tools/flake.lock");
const RUN_C: &str = include_str!("../tools/bento-run.c");

/// msb's default bind quota (4 GiB) would fail a cold build with ENOSPC.
const STORE_QUOTA_MIB: u32 = 256 * 1024;

const NIX_CONFIG: &str = "sandbox = false
build-users-group =
experimental-features = nix-command flakes
substituters = https://cache.nixos.org
trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=
";

/// Without a sandbox, nix refuses every build while the fake HOME exists, and
/// a build that writes to $HOME leaves it behind. Removed before each tool build
/// (START runs in a fresh builder, which has none).
const CLEAN_HOME: &str = "rm -rf /homeless-shelter";

const START: &str = r#"set -eu
if [ "$(cat /nix/var/bento/tools.hash 2>/dev/null)" != "__HASH__" ] || [ ! -e __TOOLS__ ]; then
  echo "builder: building box tools (first run downloads ~1 GB)..." >&2
  mkdir -p "$(dirname __TOOLS__)" /nix/var/bento
  nix build --print-build-logs path:/bento/flake#default --out-link __TOOLS__
  echo "__HASH__" > /nix/var/bento/tools.hash
fi
nohup nix-daemon >/tmp/nix-daemon.log 2>&1 </dev/null &
for i in $(seq 100); do [ -S /nix/var/nix/daemon-socket/socket ] && break; sleep 0.1; done
nohup __TOOLS__/bin/socat TCP-LISTEN:__PORT__,reuseaddr,fork \
  UNIX-CONNECT:/nix/var/nix/daemon-socket/socket </dev/null >/tmp/socat.log 2>&1 &
"#;

fn tools_hash() -> String {
    util::fingerprint(&[FLAKE, FLAKE_LOCK, RUN_C])
}

/// Everything the running builder depends on; a change recreates it.
fn spec(nix_port: u16) -> String {
    util::fingerprint(&[
        store::IMAGE,
        &nix_port.to_string(),
        NIX_CONFIG,
        START,
        &tools_hash(),
    ])
}

pub struct Builder<'a> {
    pub instance: &'a Instance,
    pub prefix: &'a str,
    pub nix_port: u16,
}

impl<'a> Builder<'a> {
    pub fn new(instance: &'a Instance, config: &'a Config) -> Self {
        Builder {
            instance,
            prefix: &config.instance.prefix,
            nix_port: config.instance.nix_port,
        }
    }

    pub fn name(&self) -> String {
        names::msb_builder(self.prefix)
    }

    fn dns_file(&self) -> PathBuf {
        self.instance.root.join("builder.dns")
    }

    /// The store attached and a healthy, current builder serving it; the
    /// host path of the store's `/nix`.
    pub async fn ensure(&self) -> Result<PathBuf> {
        let _lock = Lock::builder(self.instance)?;
        let nix = store::ensure(self.instance, self.prefix).await?;
        if let Ok(handle) = Sandbox::get(&self.name()).await {
            let reason = match self.stale(&handle)? {
                Some(reason) => reason,
                // whatever answers on the port while our builder is down is
                // someone else's daemon (another instance's, after a reboot)
                None if !crate::boxes::is_running(handle.status_snapshot()) => "not running",
                None if !healthy(self.nix_port) => "daemon not answering",
                None => return Ok(nix),
            };
            eprintln!("bento: builder: recreating ({reason})");
            util::remove_sandbox(&self.name()).await?;
        }
        self.create(&nix).await?;
        Ok(nix)
    }

    /// Recreate the builder on request. Cuts off nix builds in progress.
    pub async fn restart(&self) -> Result<()> {
        let _lock = Lock::builder(self.instance)?;
        let nix = store::ensure(self.instance, self.prefix).await?;
        util::remove_sandbox(&self.name()).await?;
        self.create(&nix).await
    }

    /// Remove the builder; the store stays.
    pub async fn remove(&self) -> Result<()> {
        let _lock = Lock::builder(self.instance)?;
        util::remove_sandbox(&self.name()).await?;
        Ok(())
    }

    fn stale(&self, handle: &SandboxHandle) -> Result<Option<&'static str>> {
        Ok(
            if util::label(handle, "bento.spec").as_deref() != Some(spec(self.nix_port).as_str()) {
                Some("bento changed")
            } else if util::dns_stale(&self.dns_file()) {
                Some("network changed")
            } else {
                None
            },
        )
    }

    /// For `bento ls`: the builder's status, None when there is none.
    pub async fn status(&self) -> Result<Option<String>> {
        let handle = match Sandbox::get(&self.name()).await {
            Ok(handle) => handle,
            Err(MicrosandboxError::SandboxNotFound(_)) => return Ok(None),
            Err(err) => return Err(err).context("looking up the builder"),
        };
        let status = format!("{:?}", handle.status_snapshot()).to_lowercase();
        Ok(Some(match self.stale(&handle)? {
            Some(why) => format!("{status} ({why})"),
            None => status,
        }))
    }

    async fn create(&self, nix: &Path) -> Result<()> {
        let port = self.nix_port;
        // a taken port fails the builder's boot
        TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|e| {
            crate::fail::fail(
                crate::fail::OTHER,
                format!("host port 127.0.0.1:{port} (instance.nix_port) is busy: {e}; the builder needs it"),
            )
        })?;
        eprintln!("bento: builder: creating '{}'", self.name());
        let sb = Sandbox::builder(self.name())
            .image(store::IMAGE)
            .user("0:0")
            .cpus(4)
            .memory(4096)
            .replace()
            .label("bento.spec", spec(port))
            .label("bento.role", "builder")
            .env("NIX_CONFIG", NIX_CONFIG)
            .volume("/nix", |m| m.bind(nix).quota(STORE_QUOTA_MIB))
            .port(port, port)
            .create_detached()
            .await
            .context("creating the builder")?;
        util::record_dns(&self.dns_file());
        sb.shell("mkdir -p /bento/flake").await?;
        sb.fs().write("/bento/flake/flake.nix", FLAKE).await?;
        sb.fs().write("/bento/flake/flake.lock", FLAKE_LOCK).await?;
        sb.fs().write("/bento/flake/bento-run.c", RUN_C).await?;
        let script = START
            .replace("__HASH__", &tools_hash())
            .replace("__TOOLS__", TOOLS)
            .replace("__PORT__", &port.to_string());
        util::run_streamed(&sb, "starting the builder", script).await?;
        for _ in 0..50 {
            if healthy(port) {
                eprintln!("bento: builder: ready");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        bail!("builder: nix-daemon does not answer on 127.0.0.1:{port} (see /tmp/nix-daemon.log via `bento builder`)")
    }

    /// Build the config's tools that changed since their last build. A tool
    /// is rebuilt only when its flake ref string changes.
    pub async fn ensure_tools(&self, config: &Config, nix: &Path) -> Result<()> {
        let _lock = Lock::builder(self.instance)?;
        let todo: Vec<&(String, String)> = config
            .tools
            .iter()
            .filter(|(name, flake)| !tool_current(nix, name, flake))
            .collect();
        if todo.is_empty() {
            return Ok(());
        }
        let sb = Sandbox::get(&self.name())
            .await
            .context("looking up the builder")?
            .connect()
            .await
            .context("connecting to the builder")?;
        for (name, flake) in todo {
            eprintln!("bento: builder: building tool {name} ({flake})");
            util::run_streamed(&sb, &format!("building tool {name}"), tool_script(name, flake))
                .await
                .map_err(|e| crate::fail::fail(crate::fail::OTHER, format!("{e:#}")))?;
        }
        Ok(())
    }

    /// Interactive shell in the builder.
    pub async fn enter(&self) -> Result<u8> {
        self.ensure().await?;
        let sb = Sandbox::get(&self.name()).await?.connect().await?;
        let code = sb
            .attach_with("bash", |a| a.cwd("/"))
            .await
            .context("attaching to the builder")?;
        Ok(if code < 0 { 0 } else { code as u8 })
    }
}

fn tool_script(name: &str, flake: &str) -> String {
    format!(
        "set -eu\nmkdir -p {EXTRA} /nix/var/bento/extra\n{CLEAN_HOME}\n\
         NIXPKGS_ALLOW_UNFREE=1 nix build --impure --print-build-logs {} --out-link {}\n\
         printf %s {} > /nix/var/bento/extra/{name}.ref\n",
        util::sh_quote(flake),
        util::sh_quote(&format!("{EXTRA}/{name}")),
        util::sh_quote(flake),
    )
}

/// Built from the same ref, and its GC root still points into the store.
fn tool_current(nix: &Path, name: &str, flake: &str) -> bool {
    let rel = |p: &str| nix.join(p.trim_start_matches("/nix/"));
    let built = std::fs::read_to_string(rel(&format!("/nix/var/bento/extra/{name}.ref")));
    let root = rel(&format!("{EXTRA}/{name}"));
    let target_ok = std::fs::read_link(&root).is_ok_and(|t| {
        t.strip_prefix("/nix/")
            .is_ok_and(|inside| nix.join(inside).exists())
    });
    built.is_ok_and(|b| b == flake) && target_ok
}

/// The daemon answers the nix worker handshake on the published port. A bare
/// TCP connect is not enough: msb accepts on the host side regardless.
fn healthy(port: u16) -> bool {
    const WORKER_MAGIC_1: u64 = 0x6e697863;
    const WORKER_MAGIC_2: u64 = 0x6478696f;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let timeout = Duration::from_secs(2);
    let Ok(mut s) = TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(timeout));
    let mut reply = [0u8; 8];
    s.write_all(&WORKER_MAGIC_1.to_le_bytes()).is_ok()
        && s.read_exact(&mut reply).is_ok()
        && u64::from_le_bytes(reply) == WORKER_MAGIC_2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_follows_the_port_and_the_tools() {
        assert_ne!(spec(17760), spec(17761));
        assert_eq!(spec(17760), spec(17760));
    }

    #[test]
    fn builds_remove_the_fake_home_first() {
        let script = tool_script("claude", "github:a/b#c");
        let clean = script.find(CLEAN_HOME).expect(&script);
        assert!(clean < script.find("nix build").unwrap(), "{script}");
    }

    #[test]
    fn tool_currency() {
        let d = std::env::temp_dir().join(format!("bento-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let nix = d.join("nix");
        std::fs::create_dir_all(nix.join("var/bento/extra")).unwrap();
        std::fs::create_dir_all(nix.join("var/nix/gcroots/bento/extra")).unwrap();
        std::fs::create_dir_all(nix.join("store/abc-claude")).unwrap();
        assert!(!tool_current(&nix, "claude", "github:a/b#c"));
        std::fs::write(nix.join("var/bento/extra/claude.ref"), "github:a/b#c").unwrap();
        assert!(!tool_current(&nix, "claude", "github:a/b#c")); // no root yet
        std::os::unix::fs::symlink(
            "/nix/store/abc-claude",
            nix.join("var/nix/gcroots/bento/extra/claude"),
        )
        .unwrap();
        assert!(tool_current(&nix, "claude", "github:a/b#c"));
        assert!(!tool_current(&nix, "claude", "github:a/b/new#c"));
        std::fs::remove_dir_all(nix.join("store/abc-claude")).unwrap();
        assert!(!tool_current(&nix, "claude", "github:a/b#c")); // collected
        let _ = std::fs::remove_dir_all(&d);
    }
}
