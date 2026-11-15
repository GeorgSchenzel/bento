//! Box lifecycle (contracts.md, Commands): create, start, restart, stop, rm,
//! ls, ports, and the boot path every booting command goes through.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use microsandbox::sandbox::{Sandbox, SandboxHandle, SandboxStatus};
use microsandbox::{MicrosandboxError, NetworkPolicy, NetworkProfile};
use microsandbox_image::ext4::{format_ext4, Ext4FormatOptions};
use serde::Serialize;

use crate::builder::Builder;
use crate::config::{self, Config};
use crate::exec;
use crate::fail::{self, fail};
use crate::guest::{self, BootInput};
use crate::instance::Instance;
use crate::names;
use crate::state::{self, Boot, BoxState, Lock, Phase, Ports};
use crate::store;
use crate::util;
use crate::workspace;

/// Bump when what a box's msb spec means changes: recreates every box at
/// its next boot (keeping its data disk).
const BOX_SPEC_VERSION: &str = "box-v2.0";
const CPUS: u8 = 4;
const MEMORY_MIB: u32 = 8192;

/// The global config for a command that may boot: any error is 125.
pub fn config_strict(instance: &Instance) -> Result<Config> {
    config::load(instance).map_err(|e| fail(fail::OTHER, format!("{e:#}")))
}

/// The global config for a command that only looks or cleans up: an error is
/// a warning (cleanup keeps working across a version skew).
pub fn config_lenient(instance: &Instance) -> Option<Config> {
    match config::load(instance) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("bento: warning: {e:#}");
            None
        }
    }
}

fn load_box(instance: &Instance, name: &str) -> Result<BoxState> {
    state::load(instance, name)?.ok_or_else(|| fail::no_box(name))
}

async fn handle(msb_name: &str) -> Result<Option<SandboxHandle>> {
    match Sandbox::get(msb_name).await {
        Ok(h) => Ok(Some(h)),
        Err(MicrosandboxError::SandboxNotFound(_)) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("looking up sandbox '{msb_name}'")),
    }
}

fn is_running(status: SandboxStatus) -> bool {
    matches!(
        status,
        SandboxStatus::Running
            | SandboxStatus::Starting
            | SandboxStatus::Paused
            | SandboxStatus::Draining
    )
}

/// `ls`'s state of a box.
pub async fn state_of(instance: &Instance, st: &BoxState) -> Result<&'static str> {
    if st.phase == Phase::Creating {
        return Ok(if Lock::box_busy(instance, &st.name)? {
            "creating"
        } else {
            "unknown"
        });
    }
    Ok(match handle(&st.msb_name).await? {
        None => "unknown",
        Some(h) if is_running(h.status_snapshot()) => "running",
        Some(_) => "stopped",
    })
}

#[derive(Serialize)]
pub struct Record {
    pub name: String,
    pub state: &'static str,
    pub config_changed: bool,
}

pub fn config_changed(st: &BoxState, config: Option<&Config>) -> bool {
    match (config, &st.boot) {
        (Some(c), Some(b)) => c.boot_hash() != b.config_hash,
        _ => false,
    }
}

pub async fn record(instance: &Instance, st: &BoxState, config: Option<&Config>) -> Result<Record> {
    Ok(Record {
        name: st.name.clone(),
        state: state_of(instance, st).await?,
        config_changed: config_changed(st, config),
    })
}

// ------------------------------------------------------------------ create

pub async fn create(
    instance: &Instance,
    name: &str,
    env: Vec<(String, String)>,
) -> Result<BoxState> {
    names::check_box(name)?;
    let config = config_strict(instance)?;
    let msb_name = names::msb_box(&config.instance.prefix, name);
    let (mut st, lock) = {
        let _ilock = Lock::instance(instance)?;
        if state::load(instance, name)?.is_some() || instance.box_dir(name).exists() {
            return Err(fail(fail::EXISTS, format!("box '{name}' exists")));
        }
        if handle(&msb_name).await?.is_some() {
            return Err(fail(
                fail::EXISTS,
                format!("a sandbox '{msb_name}' exists without a box record; `bento rm --force {name}` removes it"),
            ));
        }
        let ip = state::pick_ip(&config.instance.loopback, &state::used_ips(instance)?)
            .ok_or_else(|| fail(fail::OTHER, "no free address left in instance.loopback"))?;
        let st = BoxState {
            name: name.to_string(),
            msb_name,
            ip,
            env,
            phase: Phase::Creating,
            boot: None,
            ports: Ports::default(),
        };
        let lock = Lock::box_lock(instance, name)?;
        state::save(instance, &st)?;
        (st, lock)
    };
    let res = boot(instance, &config, &mut st).await;
    if let Err(e) = res {
        // a failed create removes what it made
        if let Ok(Some(h)) = handle(&st.msb_name).await {
            let _ = h.stop().await;
            let _ = h.remove().await;
        }
        let _ = state::remove_dir(&instance.box_dir(name));
        drop(lock);
        return Err(fail(fail::OTHER, format!("creating box '{name}': {e:#}")));
    }
    st.phase = Phase::Ready;
    state::save(instance, &st)?;
    drop(lock);
    Ok(st)
}

// ------------------------------------------------------------------- start

/// `start`: boots a stopped box; on a running one warns about what waits
/// for the next boot.
pub async fn start(instance: &Instance, name: &str) -> Result<BoxState> {
    names::check_box(name)?;
    load_box(instance, name)?;
    let config = config_strict(instance)?;
    let _lock = Lock::box_lock(instance, name)?;
    let mut st = load_box(instance, name)?;
    check_ready(&st)?;
    match running_handle(&st).await? {
        Some(sb) => {
            if !initialized(&sb).await {
                init(instance, &config, &mut st, &sb).await?;
            } else {
                refresh_ports(instance, &mut st, &sb, true).await;
            }
            warn_pending(&st, Some(&config));
        }
        None => {
            boot(instance, &config, &mut st).await?;
        }
    }
    Ok(st)
}

/// `restart`: stop, then start; a stopped box just starts.
pub async fn restart(instance: &Instance, name: &str) -> Result<BoxState> {
    names::check_box(name)?;
    load_box(instance, name)?;
    let config = config_strict(instance)?;
    let _lock = Lock::box_lock(instance, name)?;
    let mut st = load_box(instance, name)?;
    check_ready(&st)?;
    if let Some(sb) = running_handle(&st).await? {
        refresh_ports(instance, &mut st, &sb, false).await;
        halt(&st, Some(&sb)).await?;
    }
    boot(instance, &config, &mut st).await?;
    Ok(st)
}

fn check_ready(st: &BoxState) -> Result<()> {
    if st.phase == Phase::Creating {
        return Err(fail(
            fail::OTHER,
            format!(
                "box '{}' is unknown (its create was killed); `bento rm --force {}` removes it",
                st.name, st.name
            ),
        ));
    }
    Ok(())
}

/// The box, connected, when it runs.
async fn running_handle(st: &BoxState) -> Result<Option<Sandbox>> {
    let Some(h) = handle(&st.msb_name).await? else {
        return Ok(None);
    };
    if !is_running(h.status_snapshot()) {
        return Ok(None);
    }
    match h.connect().await {
        Ok(sb) => Ok(Some(sb)),
        Err(MicrosandboxError::SandboxNotRunning(_)) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("connecting to box '{}'", st.name)),
    }
}

/// A running box for exec, enter and cp: boots a stopped one through the
/// full start path unless `no_start` (then 122).
pub async fn ensure_running(
    instance: &Instance,
    name: &str,
    no_start: bool,
) -> Result<(Sandbox, BoxState, Config)> {
    names::check_box(name)?;
    load_box(instance, name)?;
    let config = config_strict(instance)?;
    let _lock = Lock::box_lock(instance, name)?;
    let mut st = load_box(instance, name)?;
    check_ready(&st)?;
    if let Some(sb) = running_handle(&st).await? {
        if !initialized(&sb).await {
            init(instance, &config, &mut st, &sb).await?;
        }
        return Ok((sb, st, config));
    }
    if no_start {
        return Err(fail(
            fail::NOT_RUNNING,
            format!("box '{name}' is not running"),
        ));
    }
    let sb = boot(instance, &config, &mut st).await?;
    Ok((sb, st, config))
}

// -------------------------------------------------------------------- boot

/// What a boot does with the box's msb sandbox.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// None yet (a new box, or one whose sandbox is gone).
    Create,
    /// The spec or the global config changed: a new sandbox, same disk.
    Recreate,
    Start,
}

pub fn plan(existing_spec: Option<Option<&str>>, want_spec: &str, config_changed: bool) -> Plan {
    match existing_spec {
        None => Plan::Create,
        Some(label) if label != Some(want_spec) || config_changed => Plan::Recreate,
        Some(_) => Plan::Start,
    }
}

/// The disk image, formatted if it doesn't exist yet. An existing one is
/// never touched: it is the box's workspace.
pub fn ensure_disk(path: &Path, gib: u64) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Ok(());
    }
    let tmp = path.with_extension("img.new");
    let _ = std::fs::remove_file(&tmp);
    let opts = Ext4FormatOptions {
        size_bytes: gib * 1024 * 1024 * 1024,
        ..Default::default()
    };
    format_ext4(&tmp, &opts).map_err(|e| anyhow!("formatting {}: {e:?}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("creating {}", path.display()))?;
    Ok(())
}

/// The msb mounts of a box, as (guest, host, kind) for the spec fingerprint.
struct Mounts {
    nix: PathBuf,
    disk: PathBuf,
    /// `files` entries: msb mount path, canonical host path, is a dir.
    files: Vec<(String, PathBuf, bool)>,
    git: Vec<(String, PathBuf)>,
    channels: Vec<(PathBuf, u32)>,
}

fn mounts(config: &Config, nix: &Path, disk: &Path) -> Result<Mounts> {
    let mut files = vec![];
    for (i, e) in config.files.iter().enumerate() {
        let host = std::fs::canonicalize(&e.host)
            .map_err(|err| fail(fail::OTHER, format!("files: {}: {err}", e.host.display())))?;
        let is_dir = host.is_dir();
        files.push((guest::file_mount(i), host, is_dir));
    }
    let mut git = vec![];
    for (i, (host, tok)) in config.git.iter().enumerate() {
        let tok = std::fs::canonicalize(tok)
            .map_err(|err| fail(fail::OTHER, format!("git.{host}: {}: {err}", tok.display())))?;
        git.push((guest::git_mount(i), tok));
    }
    for e in &config.copy {
        std::fs::metadata(&e.host)
            .map_err(|err| fail(fail::OTHER, format!("copy: {}: {err}", e.host.display())))?;
    }
    let channels = config
        .channel_ports()
        .into_iter()
        .map(|(c, port)| (c.socket.clone(), port))
        .collect();
    Ok(Mounts {
        nix: nix.to_path_buf(),
        disk: disk.to_path_buf(),
        files,
        git,
        channels,
    })
}

fn spec_of(m: &Mounts) -> String {
    let desc = format!(
        "{BOX_SPEC_VERSION}|{CPUS}|{MEMORY_MIB}|{:?}|{:?}|{:?}|{:?}|{:?}",
        m.nix, m.disk, m.files, m.git, m.channels
    );
    util::fingerprint(&[store::IMAGE, &desc])
}

/// Test-bind each declared port on the box's address: free ones get
/// published, the rest are busy (msb fails a boot on a taken port).
pub fn probe_ports(ip: Ipv4Addr, declared: &BTreeSet<u16>) -> (BTreeSet<u16>, BTreeSet<u16>) {
    let (mut free, mut busy) = (BTreeSet::new(), BTreeSet::new());
    let mut held = vec![];
    for &p in declared {
        match TcpListener::bind((ip, p)) {
            Ok(l) => {
                held.push(l);
                free.insert(p);
            }
            Err(_) => {
                busy.insert(p);
            }
        }
    }
    (free, busy)
}

/// The full start path: builder and tools, disk, the msb sandbox (created,
/// recreated or started), then `init`. A failed boot leaves the box
/// stopped, so the next boot starts over.
async fn boot(instance: &Instance, config: &Config, st: &mut BoxState) -> Result<Sandbox> {
    let builder = Builder::new(instance, config);
    let nix = builder.ensure().await?;
    builder.ensure_tools(config, &nix).await?;
    let disk = st.disk(instance);
    ensure_disk(&disk, config.disk_gib)?;
    let m = mounts(config, &nix, &disk)?;
    let want = spec_of(&m);
    let (publish, busy) = probe_ports(st.ip, &st.ports.declared);
    for p in &busy {
        eprintln!(
            "bento: warning: port {p}: {}:{p} is busy on the host, not published",
            st.ip
        );
    }
    let existing = handle(&st.msb_name).await?;
    let label = existing.as_ref().map(|h| util::label(h, "bento.spec"));
    let changed = st.boot.is_some() && config_changed(st, Some(config));
    let sb = match plan(label.as_ref().map(|l| l.as_deref()), &want, changed) {
        Plan::Start => {
            let h = existing.expect("plan Start has a sandbox");
            set_ports(&h, st.ip, &publish).await?;
            h.start_detached()
                .await
                .with_context(|| format!("starting box '{}'", st.name))?
        }
        p => {
            if let Some(h) = existing {
                if p == Plan::Recreate {
                    eprintln!(
                        "bento: box '{}': recreating (the config changed; the data disk stays)",
                        st.name
                    );
                }
                let _ = h.stop().await;
                h.remove()
                    .await
                    .with_context(|| format!("removing the old sandbox of box '{}'", st.name))?;
            }
            create_sandbox(st, &m, &want, &publish).await?
        }
    };
    st.ports.published = publish;
    st.ports.busy = busy;
    if let Err(e) = init(instance, config, st, &sb).await {
        if let Ok(Some(h)) = handle(&st.msb_name).await {
            let _ = h.stop().await;
        }
        return Err(e);
    }
    Ok(sb)
}

async fn create_sandbox(
    st: &BoxState,
    m: &Mounts,
    spec: &str,
    publish: &BTreeSet<u16>,
) -> Result<Sandbox> {
    let mut b = Sandbox::builder(st.msb_name.clone())
        .image(store::IMAGE)
        .user("0:0")
        .cpus(CPUS)
        .memory(MEMORY_MIB)
        .workdir("/")
        .label("bento.box", st.name.clone())
        .label("bento.spec", spec)
        // the builder's daemon is reached through host.microsandbox.internal
        .network(|n| {
            n.policy(NetworkPolicy::from_profiles([
                NetworkProfile::Public,
                NetworkProfile::Host,
            ]))
        })
        .volume("/nix", |v| v.bind(&m.nix).readonly())
        .volume(guest::DATA, |v| v.disk(&m.disk).fstype("ext4"));
    for (guest, host, _) in &m.files {
        b = b.volume(guest.clone(), |v| v.bind(host).readonly());
    }
    for (guest, host) in &m.git {
        b = b.volume(guest.clone(), |v| v.bind(host).readonly());
    }
    for (sock, port) in &m.channels {
        b = b.vsock(sock, *port);
    }
    for p in publish {
        b = b.port_bind(IpAddr::V4(st.ip), *p, *p);
    }
    b.create_detached()
        .await
        .with_context(|| format!("creating the sandbox of box '{}'", st.name))
}

/// Make a stopped sandbox's published ports exactly `publish` on `ip`.
async fn set_ports(h: &SandboxHandle, ip: Ipv4Addr, publish: &BTreeSet<u16>) -> Result<()> {
    let current: Vec<microsandbox_types::PublishedPortSpec> = h
        .config()
        .map(|c| c.spec.network.ports.clone())
        .unwrap_or_default();
    let want: Vec<microsandbox_types::PublishedPortSpec> = publish
        .iter()
        .map(|p| microsandbox_types::PublishedPortSpec {
            host_port: *p,
            guest_port: *p,
            host_bind: ip.to_string(),
            ..Default::default()
        })
        .collect();
    let same = current.len() == want.len()
        && want.iter().all(|w| {
            current.iter().any(|c| {
                c.host_port == w.host_port
                    && c.guest_port == w.guest_port
                    && c.host_bind == w.host_bind
            })
        });
    if same {
        return Ok(());
    }
    let mut m = h.modify().next_start();
    // msb refuses a port that is both set and removed: remove only the
    // ones no longer wanted (setting a kept one replaces it)
    for c in stale_ports(&current, &want) {
        m = m.remove_port(microsandbox::sandbox::PublishedPortKey {
            host_bind: c.host_bind.clone(),
            host_port: c.host_port,
            protocol: c.protocol,
        });
    }
    for w in want {
        m = m.port_mapping(w);
    }
    m.apply().await.context("setting the box's ports")?;
    Ok(())
}

/// The published ports in `current` that `want` doesn't keep (same host
/// address, port and protocol).
fn stale_ports<'a>(
    current: &'a [microsandbox_types::PublishedPortSpec],
    want: &[microsandbox_types::PublishedPortSpec],
) -> Vec<&'a microsandbox_types::PublishedPortSpec> {
    current
        .iter()
        .filter(|c| {
            !want.iter().any(|w| {
                w.host_bind == c.host_bind && w.host_port == c.host_port && w.protocol == c.protocol
            })
        })
        .collect()
}

/// The host's git identity, if it has one.
fn git_identity() -> Option<(String, String)> {
    let get = |key: &str| {
        let out = std::process::Command::new("git")
            .args(["config", "--global", "--get", key])
            .output()
            .ok()?;
        let v = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (out.status.success() && !v.is_empty()).then_some(v)
    };
    Some((get("user.name")?, get("user.email")?))
}

/// The boot script's marker: this boot's id, written last.
const BOOTED: &str = "/run/bento-sys/booted";

async fn initialized(sb: &Sandbox) -> bool {
    let check =
        format!("[ \"$(cat {BOOTED} 2>/dev/null)\" = \"$(cat /proc/sys/kernel/random/boot_id)\" ]");
    matches!(exec::script(sb, &check, &[], &[]).await, Ok(o) if o.code == 0)
}

/// Everything bento does in a freshly booted box, before anything else runs.
async fn init(instance: &Instance, config: &Config, st: &mut BoxState, sb: &Sandbox) -> Result<()> {
    let home = &instance.user_home;
    let own = guest::own_env(config, &st.name, home);
    let env = guest::merge_env(&[&own, &config.env, &st.env]);
    let input = BootInput {
        config,
        home,
        files_are_dirs: config
            .files
            .iter()
            .map(|e| std::fs::metadata(&e.host).is_ok_and(|m| m.is_dir()))
            .collect(),
        identity: git_identity(),
    };
    exec::script(sb, &guest::boot_script(&input), &[], &env)
        .await?
        .check("booting the box")
        .map_err(|e| fail(fail::OTHER, format!("{e:#}")))?;
    for e in &config.copy {
        copy_in(sb, &e.host, &e.guest, &env).await?;
    }
    exec::script(
        sb,
        &format!("cat /proc/sys/kernel/random/boot_id > {BOOTED}"),
        &[],
        &env,
    )
    .await?
    .check("booting the box")?;
    st.boot = Some(Boot {
        config_hash: config.boot_hash(),
        env: guest::merge_env(&[&own, &config.env]),
        env_files: config.env_files.clone(),
    });
    state::save(instance, st)?;
    refresh_ports(instance, st, sb, true).await;
    warn_pending(st, None);
    Ok(())
}

/// A `copy` entry: the host file or dir, replacing the guest path.
async fn copy_in(
    sb: &Sandbox,
    host: &Path,
    guest_path: &str,
    env: &[(String, String)],
) -> Result<()> {
    let dir = crate::cp::parent_of(guest_path);
    let name = crate::cp::base_name(guest_path);
    let host = host.to_path_buf();
    let n = name.clone();
    exec::script_with_input(sb, guest::COPY_IN, &[&dir, &name], env, move |w| {
        crate::cp::write_archive_following_top(&host, &n, w)
    })
    .await?
    .check(&format!("copying {guest_path} in"))
    .map_err(|e| fail(fail::OTHER, format!("{e:#}")))?;
    Ok(())
}

/// Re-read the workspace's `bento.yaml` files into the record. Best effort:
/// a box that can't answer keeps what was last read. With `show`, the
/// files' warnings are printed.
async fn refresh_ports(instance: &Instance, st: &mut BoxState, sb: &Sandbox, show: bool) {
    let read = tokio::time::timeout(
        Duration::from_secs(10),
        exec::script(sb, workspace::READ_SCRIPT, &[], &[]),
    )
    .await;
    let Ok(Ok(out)) = read else {
        eprintln!("bento: warning: couldn't read the workspace's bento.yaml files");
        return;
    };
    let declared = workspace::parse_listing(&out.stdout);
    if show {
        for w in &declared.warnings {
            eprintln!("bento: warning: {w}");
        }
    }
    st.ports.declared = declared.ports;
    if let Err(e) = state::save(instance, st) {
        eprintln!("bento: warning: {e:#}");
    }
}

fn warn_pending(st: &BoxState, config: Option<&Config>) {
    let pending: Vec<String> = st
        .ports
        .declared
        .iter()
        .filter(|p| !st.ports.published.contains(p) && !st.ports.busy.contains(p))
        .map(|p| p.to_string())
        .collect();
    if !pending.is_empty() {
        eprintln!(
            "bento: warning: box '{}': ports {} are pending: they're published at the next boot (`bento restart {}`)",
            st.name,
            pending.join(", "),
            st.name
        );
    }
    if config_changed(st, config) {
        eprintln!(
            "bento: warning: box '{}': the global config changed since its boot; it applies at the next boot (`bento restart {}`)",
            st.name, st.name
        );
    }
}

// -------------------------------------------------------------- stop and rm

/// Stop a running box: its processes get SIGHUP, then SIGKILL after 5 s,
/// then the VM stops.
async fn halt(st: &BoxState, sb: Option<&Sandbox>) -> Result<()> {
    if let Some(sb) = sb {
        let _ = tokio::time::timeout(
            Duration::from_secs(15),
            exec::script(sb, guest::HANGUP_ALL, &[], &[]),
        )
        .await;
    }
    let Some(h) = handle(&st.msb_name).await? else {
        return Ok(());
    };
    if !is_running(h.status_snapshot()) {
        return Ok(());
    }
    if let Err(e) = h.stop_with_timeout(Duration::from_secs(30)).await {
        eprintln!(
            "bento: warning: stopping box '{}': {e}; killing it",
            st.name
        );
        h.kill()
            .await
            .with_context(|| format!("stopping box '{}'", st.name))?;
    }
    Ok(())
}

pub async fn stop(instance: &Instance, name: &str) -> Result<()> {
    names::check_box(name)?;
    load_box(instance, name)?;
    let _lock = Lock::box_lock(instance, name)?;
    let mut st = load_box(instance, name)?;
    if let Some(sb) = running_handle(&st).await? {
        refresh_ports(instance, &mut st, &sb, false).await;
        halt(&st, Some(&sb)).await?;
    }
    Ok(())
}

pub async fn rm(instance: &Instance, name: &str, force: bool) -> Result<()> {
    names::check_box(name)?;
    let _lock = Lock::box_lock(instance, name)?;
    let st = state::load(instance, name);
    let st = match (st, force) {
        (Ok(Some(st)), _) => Some(st),
        (Ok(None), _) if !force && !instance.box_dir(name).exists() => {
            return Err(fail::no_box(name))
        }
        (Ok(None), _) => None,
        (Err(e), false) => {
            return Err(fail(
                fail::OTHER,
                format!("{e:#}; `bento rm --force {name}` removes it anyway"),
            ))
        }
        (Err(e), true) => {
            eprintln!("bento: warning: {e:#}");
            None
        }
    };
    // the sandbox: by the record's name, else (--force) the config's prefix
    let msb_name = match &st {
        Some(st) => Some(st.msb_name.clone()),
        None => config_lenient(instance).map(|c| names::msb_box(&c.instance.prefix, name)),
    };
    let step = |res: Result<()>| -> Result<()> {
        match res {
            Err(e) if force => {
                eprintln!("bento: warning: {e:#}");
                Ok(())
            }
            Err(e) => Err(fail(
                fail::OTHER,
                format!("{e:#}; nothing removed (`bento rm --force {name}` removes it anyway)"),
            )),
            ok => ok,
        }
    };
    if let Some(msb_name) = &msb_name {
        let h = match handle(msb_name).await {
            Ok(h) => h,
            Err(e) => {
                step(Err(e))?;
                None
            }
        };
        if let Some(h) = h {
            if is_running(h.status_snapshot()) {
                let stopping = async {
                    let sb = h.connect().await.ok();
                    let fake = st.clone().unwrap_or_else(|| BoxState {
                        name: name.into(),
                        msb_name: msb_name.clone(),
                        ip: Ipv4Addr::UNSPECIFIED,
                        env: vec![],
                        phase: Phase::Ready,
                        boot: None,
                        ports: Ports::default(),
                    });
                    halt(&fake, sb.as_ref()).await
                };
                step(stopping.await)?;
            }
            step(h.remove().await.context("removing the sandbox"))?;
        }
    } else if !force {
        bail!("no config to find the sandbox by");
    }
    step(state::remove_dir(&instance.box_dir(name)))?;
    Ok(())
}

// ------------------------------------------------------------- ls, ports

pub async fn ls(instance: &Instance) -> Result<Vec<Record>> {
    let config = config_lenient(instance);
    let mut out = vec![];
    for name in state::names(instance)? {
        match state::load(instance, &name) {
            Ok(Some(st)) => out.push(record(instance, &st, config.as_ref()).await?),
            Ok(None) => out.push(Record {
                name,
                state: "unknown",
                config_changed: false,
            }),
            Err(e) => {
                eprintln!("bento: warning: {e:#}");
                out.push(Record {
                    name,
                    state: "unknown",
                    config_changed: false,
                });
            }
        }
    }
    Ok(out)
}

#[derive(Serialize)]
pub struct PortsReport {
    #[serde(rename = "box")]
    pub name: String,
    pub running: bool,
    pub ports: Vec<workspace::PortView>,
}

pub async fn ports(instance: &Instance, name: &str) -> Result<PortsReport> {
    names::check_box(name)?;
    let _ = config_lenient(instance);
    let mut st = load_box(instance, name)?;
    let sb = running_handle(&st).await?;
    if let Some(sb) = &sb {
        let read = exec::script(sb, workspace::READ_SCRIPT, &[], &[]).await?;
        let declared = workspace::parse_listing(&read.stdout);
        for w in &declared.warnings {
            eprintln!("bento: warning: {w}");
        }
        st.ports.declared = declared.ports;
        // record it for the next boot, unless a lifecycle command is busy
        if !Lock::box_busy(instance, name)? {
            if let Ok(_l) = Lock::box_lock(instance, name) {
                if let Ok(Some(mut cur)) = state::load(instance, name) {
                    cur.ports.declared = st.ports.declared.clone();
                    let _ = state::save(instance, &cur);
                }
            }
        }
    }
    Ok(PortsReport {
        name: st.name.clone(),
        running: sb.is_some(),
        ports: workspace::port_views(&st.ports, st.ip, sb.is_some()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_plans() {
        assert_eq!(plan(None, "s", false), Plan::Create);
        assert_eq!(plan(Some(Some("s")), "s", false), Plan::Start);
        assert_eq!(plan(Some(Some("old")), "s", false), Plan::Recreate);
        assert_eq!(plan(Some(None), "s", false), Plan::Recreate);
        assert_eq!(plan(Some(Some("s")), "s", true), Plan::Recreate);
    }

    #[test]
    fn the_data_disk_is_never_reformatted() {
        let d = std::env::temp_dir().join(format!("bento-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let disk = d.join("disk.img");
        ensure_disk(&disk, 1).unwrap();
        let meta = std::fs::metadata(&disk).unwrap();
        assert_eq!(meta.len(), 1 << 30);
        use std::os::unix::fs::MetadataExt as _;
        // sparse: far less allocated than its size
        assert!(
            meta.blocks() * 512 < 256 << 20,
            "{} bytes allocated",
            meta.blocks() * 512
        );
        // what the box wrote stays, whatever the size asked
        std::fs::write(&disk, b"workspace").unwrap();
        ensure_disk(&disk, 64).unwrap();
        assert_eq!(std::fs::read(&disk).unwrap(), b"workspace");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_dropped_ports_are_removed() {
        let port = |p: u16| microsandbox_types::PublishedPortSpec {
            host_port: p,
            guest_port: p,
            host_bind: "127.78.0.1".into(),
            ..Default::default()
        };
        let current = [port(5173), port(8080)];
        let want = [port(5173), port(9000)];
        let stale: Vec<u16> = stale_ports(&current, &want).iter().map(|p| p.host_port).collect();
        assert_eq!(stale, vec![8080]);
    }

    #[test]
    fn busy_ports_are_left_out() {
        let ip = Ipv4Addr::new(127, 0, 0, 1);
        let taken = TcpListener::bind((ip, 0)).unwrap();
        let busy_port = taken.local_addr().unwrap().port();
        let free_port = {
            let l = TcpListener::bind((ip, 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let (free, busy) = probe_ports(ip, &BTreeSet::from([busy_port, free_port]));
        assert_eq!(busy, BTreeSet::from([busy_port]));
        assert_eq!(free, BTreeSet::from([free_port]));
    }

    #[test]
    fn mounts_never_include_a_writable_host_dir() {
        // the spec's mounts: the store read-only, the disk image, read-only
        // files and token files; nothing else from the host
        let d = std::env::temp_dir().join(format!("bento-mounts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("cfgdir")).unwrap();
        std::fs::write(d.join("tok"), "t").unwrap();
        let inst = Instance::resolve(None, Some(d.as_os_str())).unwrap();
        let text = format!(
            "files: [{{guest: /etc/x, host: {0}/cfgdir}}]\ngit: {{github.com: {0}/tok}}\ncopy: [{{guest: /etc/y, host: {0}/tok}}]",
            d.display()
        );
        let c = config::parse(Some(&text), &inst).unwrap();
        let m = mounts(&c, Path::new("/n"), Path::new("/d.img")).unwrap();
        assert_eq!(m.files.len(), 1);
        assert!(m.files[0].2);
        assert_eq!(m.git.len(), 1);
        // a missing host path fails the boot
        let bad = config::parse(
            Some("files: [{guest: /etc/x, host: /nonexistent/bento}]"),
            &inst,
        )
        .unwrap();
        assert_eq!(
            fail::code_of(
                &mounts(&bad, Path::new("/n"), Path::new("/d"))
                    .err()
                    .unwrap()
            ),
            fail::OTHER
        );
        // the spec changes with the mounts
        let c2 = config::parse(Some("channels: {wok: {socket: /w}}"), &inst).unwrap();
        let m2 = mounts(&c2, Path::new("/n"), Path::new("/d.img")).unwrap();
        assert_ne!(spec_of(&m), spec_of(&m2));
        let _ = std::fs::remove_dir_all(&d);
    }
}
