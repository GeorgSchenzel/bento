//! The builder: one sandbox per host, the only writer of the shared store.
//! It runs nix-daemon behind a socat TCP listener published on 127.0.0.1;
//! boxes mount the store ro and reach the daemon through
//! host.microsandbox.internal. It is never restarted (published ports and
//! background processes don't survive stop→start) — when unhealthy or
//! outdated it is recreated.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use microsandbox::sandbox::Sandbox;

use crate::{store, util};

pub const NAME: &str = "bento-builder";
pub const PORT: u16 = 17760;

/// GC root of the box tools (embedded flake's `packages.default`).
pub const TOOLS: &str = "/nix/var/nix/gcroots/bento/tools";

const FLAKE: &str = include_str!("../tools/flake.nix");
const FLAKE_LOCK: &str = include_str!("../tools/flake.lock");

/// Generous: the store volume is capped by its sparse image instead. msb's
/// default bind quota (4 GiB) would fail a cold build with ENOSPC.
const STORE_QUOTA_MIB: u32 = 64 * 1024;

const NIX_CONFIG: &str = "sandbox = false
build-users-group =
experimental-features = nix-command flakes
substituters = https://cache.nixos.org
trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=
";

const START: &str = r#"set -eu
if [ "$(cat /nix/var/bento/tools.hash 2>/dev/null)" != "__HASH__" ] || [ ! -e __TOOLS__ ]; then
  echo "builder: building box tools (first run downloads ~1 GB)..."
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
    util::fingerprint(&[FLAKE, FLAKE_LOCK])
}

/// Everything the running builder depends on; a change recreates it.
fn spec() -> String {
    util::fingerprint(&[store::IMAGE, &PORT.to_string(), NIX_CONFIG, START, &tools_hash()])
}

/// Make sure the store is attached and a healthy, current builder serves it.
/// Holds a host lock so concurrent bento commands don't race to recreate it.
pub async fn ensure() -> Result<PathBuf> {
    let lock_path = util::data_dir()?.join("builder.lock");
    let lock = File::create(&lock_path).with_context(|| format!("opening {}", lock_path.display()))?;
    lock.lock().context("locking the builder")?;

    let nix = store::ensure().await?;
    let spec = spec();
    if let Ok(handle) = Sandbox::get(NAME).await {
        let reason = if util::label(&handle, "bento.spec").as_deref() != Some(spec.as_str()) {
            "bento changed"
        } else if healthy() {
            return Ok(nix);
        } else {
            "daemon not answering"
        };
        println!("builder: recreating ({reason})");
        util::remove_sandbox(NAME).await?;
    }
    create(&nix, &spec).await?;
    Ok(nix)
}

async fn create(nix: &Path, spec: &str) -> Result<()> {
    // msb silently drops a published port that is already taken
    TcpListener::bind((Ipv4Addr::LOCALHOST, PORT))
        .with_context(|| format!("host port 127.0.0.1:{PORT} is in use; the builder needs it"))?;
    println!("builder: creating '{NAME}'");
    let sb = Sandbox::builder(NAME)
        .image(store::IMAGE)
        .user("0:0")
        .cpus(4)
        .memory(4096)
        .replace()
        .label("bento.spec", spec)
        .env("NIX_CONFIG", NIX_CONFIG)
        .volume("/nix", |m| m.bind(nix).quota(STORE_QUOTA_MIB))
        .port(PORT, PORT)
        .create_detached()
        .await
        .context("creating the builder")?;
    sb.shell("mkdir -p /bento/flake").await?;
    sb.fs().write("/bento/flake/flake.nix", FLAKE).await?;
    sb.fs().write("/bento/flake/flake.lock", FLAKE_LOCK).await?;
    let script = START
        .replace("__HASH__", &tools_hash())
        .replace("__TOOLS__", TOOLS)
        .replace("__PORT__", &PORT.to_string());
    util::run_streamed(&sb, "starting the builder", script).await?;
    for _ in 0..50 {
        if healthy() {
            println!("builder: ready");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("builder: nix-daemon does not answer on 127.0.0.1:{PORT} (see /tmp/nix-daemon.log via `bento builder`)")
}

/// The daemon answers the nix worker handshake on the published port. A bare
/// TCP connect is not enough: msb accepts on the host side regardless.
fn healthy() -> bool {
    const WORKER_MAGIC_1: u64 = 0x6e697863;
    const WORKER_MAGIC_2: u64 = 0x6478696f;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, PORT));
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

/// Interactive shell in the builder.
pub async fn enter() -> Result<u8> {
    ensure().await?;
    let sb = Sandbox::get(NAME).await?.connect().await?;
    util::attach(&sb, "bash", &[], "/", util::term_env()).await
}
