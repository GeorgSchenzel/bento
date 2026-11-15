//! The global config (contracts.md, Global config): parsed strictly, since a
//! typo must not silently boot a box without a channel or a credential.

use std::net::Ipv4Addr;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use serde::Serialize;
use serde_yaml::Value;

use crate::instance::Instance;
use crate::names;
use crate::util;

pub const DEFAULT_PREFIX: &str = "bento-";
pub const DEFAULT_LOOPBACK: &str = "127.77.0.0/24";
pub const DEFAULT_NIX_PORT: u16 = 17760;
pub const DEFAULT_DISK_GIB: u64 = 64;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Config {
    pub instance: InstanceConfig,
    pub disk_gib: u64,
    /// Sorted by name (their vsock ports follow that order).
    pub channels: Vec<Channel>,
    /// In config order: earlier tools come first on PATH.
    pub tools: Vec<(String, String)>,
    pub files: Vec<Entry>,
    pub copy: Vec<Entry>,
    pub env: Vec<(String, String)>,
    pub env_files: Vec<(String, PathBuf)>,
    /// Git host (as in `https://<host>/…`) → token file.
    pub git: Vec<(String, PathBuf)>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstanceConfig {
    pub prefix: String,
    pub loopback: Loopback,
    pub nix_port: u16,
}

/// An IPv4 range inside 127.0.0.0/8, one address per box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Loopback {
    pub network: Ipv4Addr,
    pub len: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Channel {
    pub name: String,
    pub socket: PathBuf,
}

/// A `files` or `copy` entry: a guest path (absolute, `~` expanded to the
/// host's HOME) and a host path.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub guest: String,
    pub host: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            instance: InstanceConfig {
                prefix: DEFAULT_PREFIX.into(),
                loopback: Loopback::parse(DEFAULT_LOOPBACK).expect("default loopback"),
                nix_port: DEFAULT_NIX_PORT,
            },
            disk_gib: DEFAULT_DISK_GIB,
            channels: vec![],
            tools: vec![],
            files: vec![],
            copy: vec![],
            env: vec![],
            env_files: vec![],
            git: vec![],
        }
    }
}

impl Loopback {
    pub fn parse(s: &str) -> Result<Loopback> {
        let (addr, len) = s
            .split_once('/')
            .ok_or_else(|| anyhow!("loopback '{s}': expected <address>/<length>"))?;
        let addr: Ipv4Addr = addr
            .parse()
            .map_err(|_| anyhow!("loopback '{s}': bad address"))?;
        let len: u8 = len
            .parse()
            .map_err(|_| anyhow!("loopback '{s}': bad length"))?;
        if !(8..=30).contains(&len) {
            bail!("loopback '{s}': the length must be 8 to 30");
        }
        let mask = u32::MAX << (32 - len);
        let network = Ipv4Addr::from(u32::from(addr) & mask);
        if network.octets()[0] != 127 {
            bail!("loopback '{s}': must be inside 127.0.0.0/8");
        }
        if u32::from(addr) != u32::from(network) {
            bail!("loopback '{s}': has host bits set (the network is {network}/{len})");
        }
        Ok(Loopback { network, len })
    }

    /// Addresses for boxes: the range without its network and broadcast
    /// addresses, in order.
    pub fn hosts(&self) -> impl Iterator<Item = Ipv4Addr> {
        let base = u32::from(self.network);
        let size = 1u32 << (32 - self.len);
        (1..size - 1).map(move |i| Ipv4Addr::from(base + i))
    }
}

/// Read and check the config of `instance`. A missing file is the defaults,
/// unless `BENTO_HOME` is set (then it must exist and set `instance`).
pub fn load(instance: &Instance) -> Result<Config> {
    let path = instance.config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    parse(text.as_deref(), instance).with_context(|| format!("config {}", path.display()))
}

/// Parse the config text (None: no file).
pub fn parse(text: Option<&str>, instance: &Instance) -> Result<Config> {
    let home = &instance.user_home;
    let root = match text {
        None if instance.explicit => {
            bail!("missing; BENTO_HOME is set, so it must exist and set `instance`")
        }
        None => return Ok(Config::default()),
        Some(text) => serde_yaml::from_str::<Value>(text).context("invalid YAML")?,
    };
    let mut config = Config::default();
    let root = match root {
        Value::Null => {
            if instance.explicit {
                bail!("`instance` must be set when BENTO_HOME is set");
            }
            return Ok(config);
        }
        Value::Mapping(m) => m,
        _ => bail!("expected a mapping at the top"),
    };
    let mut saw_instance = false;
    for (key, value) in root {
        let key = key_str(&key)?;
        match key.as_str() {
            "instance" => {
                saw_instance = true;
                config.instance = parse_instance(value)?;
            }
            "disk_gib" => {
                config.disk_gib = value
                    .as_u64()
                    .filter(|n| (1..=16 * 1024).contains(n))
                    .ok_or_else(|| {
                        anyhow!("disk_gib: expected a whole number of GiB, 1 to 16384")
                    })?;
            }
            "channels" => config.channels = parse_channels(value, home)?,
            "tools" => config.tools = parse_tools(value)?,
            "files" => config.files = parse_entries("files", value, home)?,
            "copy" => config.copy = parse_entries("copy", value, home)?,
            "env" => config.env = parse_env(value)?,
            "env_files" => config.env_files = parse_env_files(value, home)?,
            "git" => config.git = parse_git(value, home)?,
            other => bail!("unknown key `{other}`"),
        }
    }
    if instance.explicit && !saw_instance {
        bail!("`instance` must be set when BENTO_HOME is set");
    }
    check_overlaps(&config)?;
    for (name, _) in &config.env_files {
        if config.env.iter().any(|(k, _)| k == name) {
            bail!("{name} is in both env and env_files");
        }
    }
    Ok(config)
}

impl Config {
    /// What a box's boot depends on: a change shows as `config_changed` and
    /// recreates a stopped box at its next boot. `disk_gib` only applies to
    /// new boxes.
    pub fn boot_hash(&self) -> String {
        let mut c = self.clone();
        c.disk_gib = 0;
        util::fingerprint(&[&serde_json::to_string(&c).expect("config serializes")])
    }

    /// The vsock port of each channel, in name order.
    pub fn channel_ports(&self) -> Vec<(&Channel, u32)> {
        self.channels
            .iter()
            .enumerate()
            .map(|(i, c)| (c, CHANNEL_PORT_BASE + i as u32))
            .collect()
    }
}

/// vsock ports for channels start here.
pub const CHANNEL_PORT_BASE: u32 = 6000;

fn key_str(key: &Value) -> Result<String> {
    key.as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("keys must be strings, got {key:?}"))
}

fn scalar_str(what: &str, v: &Value) -> Result<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => bail!("{what}: expected a string"),
    }
}

fn mapping(what: &str, v: Value) -> Result<serde_yaml::Mapping> {
    match v {
        Value::Mapping(m) => Ok(m),
        Value::Null => Ok(Default::default()),
        _ => bail!("{what}: expected a mapping"),
    }
}

fn parse_instance(v: Value) -> Result<InstanceConfig> {
    let mut inst = Config::default().instance;
    for (key, value) in mapping("instance", v)? {
        match key_str(&key)?.as_str() {
            "prefix" => {
                let p = scalar_str("instance.prefix", &value)?;
                names::check_prefix(&p).map_err(|e| anyhow!(e))?;
                inst.prefix = p;
            }
            "loopback" => {
                inst.loopback = Loopback::parse(&scalar_str("instance.loopback", &value)?)
                    .context("instance.loopback")?
            }
            "nix_port" => {
                inst.nix_port = value
                    .as_u64()
                    .and_then(|n| u16::try_from(n).ok())
                    .filter(|n| *n >= 1024)
                    .ok_or_else(|| anyhow!("instance.nix_port: expected a port, 1024 to 65535"))?
            }
            other => bail!("unknown key `instance.{other}`"),
        }
    }
    Ok(inst)
}

/// `~` and `~/…` are the host's HOME; anything else must be absolute. `..`
/// is refused, `.` and repeated slashes are dropped.
pub fn expand(what: &str, path: &str, home: &Path) -> Result<PathBuf> {
    let p = if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    };
    if !p.is_absolute() {
        bail!("{what} '{path}': must be absolute or start with ~/");
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => bail!("{what} '{path}': '..' isn't allowed"),
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    Ok(out)
}

fn parse_channels(v: Value, home: &Path) -> Result<Vec<Channel>> {
    let mut out = Vec::new();
    for (key, value) in mapping("channels", v)? {
        let name = key_str(&key)?;
        let ok = !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            && name.as_bytes()[0].is_ascii_alphanumeric();
        if !ok {
            bail!("channels: bad name '{name}' (lowercase letters, digits, '-', '_', at most 32)");
        }
        let mut socket = None;
        for (k, v) in mapping(&format!("channels.{name}"), value)? {
            match key_str(&k)?.as_str() {
                "socket" => {
                    let s = scalar_str(&format!("channels.{name}.socket"), &v)?;
                    socket = Some(expand(&format!("channels.{name}.socket"), &s, home)?);
                }
                other => bail!("unknown key `channels.{name}.{other}`"),
            }
        }
        let socket = socket.ok_or_else(|| anyhow!("channels.{name}: `socket` is missing"))?;
        out.push(Channel { name, socket });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn parse_tools(v: Value) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (key, value) in mapping("tools", v)? {
        let name = key_str(&key)?;
        let ok = !name.is_empty()
            && name.len() <= 64
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if !ok {
            bail!("tools: bad name '{name}'");
        }
        let flake = scalar_str(&format!("tools.{name}"), &value)?;
        check_flake_ref(&name, &flake)?;
        out.push((name, flake));
    }
    Ok(out)
}

/// A pinned flake ref the builder can fetch itself: never a local path (the
/// builder sees no host paths) and never something `nix build` reads as a flag.
pub fn check_flake_ref(name: &str, flake: &str) -> Result<()> {
    if flake.is_empty() || flake.starts_with('-') || flake.chars().any(char::is_whitespace) {
        bail!("tools.{name}: '{flake}' is not a flake ref");
    }
    for local in [
        "/",
        ".",
        "~",
        "path:",
        "file:",
        "git+file:",
        "tarball+file:",
    ] {
        if flake.starts_with(local) {
            bail!("tools.{name}: '{flake}' is a local path; the builder only fetches remote refs");
        }
    }
    if !flake.contains('#') {
        bail!("tools.{name}: '{flake}' names no package (expected <flake>#<package>)");
    }
    Ok(())
}

fn parse_entries(what: &str, v: Value, home: &Path) -> Result<Vec<Entry>> {
    let list = match v {
        Value::Sequence(s) => s,
        Value::Null => vec![],
        _ => bail!("{what}: expected a list of {{guest, host}}"),
    };
    let mut out = Vec::new();
    for (i, item) in list.into_iter().enumerate() {
        let (mut guest, mut host) = (None, None);
        for (k, v) in mapping(&format!("{what}[{i}]"), item)? {
            match key_str(&k)?.as_str() {
                "guest" => guest = Some(scalar_str(&format!("{what}[{i}].guest"), &v)?),
                "host" => host = Some(scalar_str(&format!("{what}[{i}].host"), &v)?),
                other => bail!("unknown key `{what}[{i}].{other}`"),
            }
        }
        let guest = guest.ok_or_else(|| anyhow!("{what}[{i}]: `guest` is missing"))?;
        let host = host.ok_or_else(|| anyhow!("{what}[{i}]: `host` is missing"))?;
        let guest_path = expand(&format!("{what}[{i}].guest"), &guest, home)?;
        check_guest_path(&format!("{what}[{i}].guest"), &guest_path, home)?;
        let guest = guest_path
            .to_str()
            .ok_or_else(|| anyhow!("{what}[{i}].guest: not UTF-8"))?
            .to_string();
        let host = expand(&format!("{what}[{i}].host"), &host, home)?;
        out.push(Entry { guest, host });
    }
    Ok(out)
}

/// Guest paths bento itself owns or that would hide the data disk.
fn check_guest_path(what: &str, guest: &Path, home: &Path) -> Result<()> {
    for exact in [
        Path::new("/"),
        Path::new("/workspace"),
        home,
        Path::new("/var/lib/docker"),
    ] {
        if guest == exact {
            bail!("{what} '{}': would hide a data disk dir", guest.display());
        }
    }
    for owned in [
        "/nix",
        "/bento",
        "/run/bento",
        "/run/bento-sys",
        "/proc",
        "/sys",
        "/dev",
        "/etc/bento",
    ] {
        if guest.starts_with(owned) {
            bail!(
                "{what} '{}': {owned} is bento's or the kernel's",
                guest.display()
            );
        }
    }
    Ok(())
}

fn check_overlaps(config: &Config) -> Result<()> {
    let all: Vec<&Entry> = config.files.iter().chain(&config.copy).collect();
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            let (pa, pb) = (Path::new(&a.guest), Path::new(&b.guest));
            if pa.starts_with(pb) || pb.starts_with(pa) {
                bail!(
                    "files/copy: guest paths '{}' and '{}' overlap",
                    a.guest,
                    b.guest
                );
            }
        }
    }
    Ok(())
}

fn parse_env(v: Value) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (key, value) in mapping("env", v)? {
        let name = env_name("env", &key)?;
        out.push((name.clone(), scalar_str(&format!("env.{name}"), &value)?));
    }
    Ok(out)
}

fn env_name(what: &str, key: &Value) -> Result<String> {
    let name = key_str(key)?;
    if !names::is_env_name(&name) {
        bail!("{what}: '{name}' is not an env name");
    }
    if names::is_reserved_env(&name) {
        bail!("{what}: {name} is set by bento");
    }
    Ok(name)
}

fn parse_env_files(v: Value, home: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for (key, value) in mapping("env_files", v)? {
        let name = env_name("env_files", &key)?;
        let path = scalar_str(&format!("env_files.{name}"), &value)?;
        out.push((
            name.clone(),
            expand(&format!("env_files.{name}"), &path, home)?,
        ));
    }
    Ok(out)
}

fn parse_git(v: Value, home: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for (key, value) in mapping("git", v)? {
        let host = key_str(&key)?;
        if !is_git_host(&host) {
            bail!("git: '{host}' is not a host name");
        }
        let path = scalar_str(&format!("git.{host}"), &value)?;
        out.push((host.clone(), expand(&format!("git.{host}"), &path, home)?));
    }
    Ok(out)
}

/// `host` or `host:port`, letters, digits, '.' and '-'.
pub fn is_git_host(host: &str) -> bool {
    let (name, port) = match host.split_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (host, None),
    };
    let name_ok = !name.is_empty()
        && name.len() <= 253
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    let port_ok = port.is_none_or(|p| p.parse::<u16>().is_ok_and(|n| n > 0));
    name_ok && port_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn default_instance() -> Instance {
        Instance::resolve(None, Some(OsStr::new("/home/u"))).unwrap()
    }

    fn explicit() -> Instance {
        Instance::resolve(Some(OsStr::new("/tmp/b")), Some(OsStr::new("/home/u"))).unwrap()
    }

    fn ok(text: &str) -> Config {
        parse(Some(text), &default_instance()).unwrap()
    }

    fn err(text: &str) -> String {
        format!("{:#}", parse(Some(text), &default_instance()).unwrap_err())
    }

    const FULL: &str = r#"
instance:
  prefix: bento-
  loopback: 127.77.0.0/24
  nix_port: 17760
disk_gib: 64
channels:
  wok:  {socket: ~/.local/share/wok/wok.sock}
  stew: {socket: /run/user/1000/stew.sock}
tools:
  claude: github:o/r/abc#claude-code
  hc: github:o/hc/def#hc
files:
  - {guest: ~/.claude/skills, host: ~/.local/share/stew/skills}
  - {guest: ~/.config/nvim, host: ~/.config/nvim}
copy:
  - {guest: ~/.local/share/opencode/auth.json, host: ~/.local/share/opencode/auth.json}
env:
  STEW_HUB: unix:/run/bento/stew.sock
  N: 3
env_files:
  CLAUDE_CODE_OAUTH_TOKEN: ~/.config/bento/secrets/claude
git:
  github.com: ~/.config/bento/secrets/github
  gitlab.example.com:8443: /etc/tok
"#;

    #[test]
    fn contract_example_parses() {
        let c = ok(FULL);
        assert_eq!(c.instance.prefix, "bento-");
        assert_eq!(c.instance.nix_port, 17760);
        // sorted by name, ~ expanded
        assert_eq!(c.channels[0].name, "stew");
        assert_eq!(
            c.channels[1].socket,
            Path::new("/home/u/.local/share/wok/wok.sock")
        );
        assert_eq!(c.tools[0].0, "claude");
        assert_eq!(c.files[0].guest, "/home/u/.claude/skills");
        assert_eq!(
            c.files[0].host,
            Path::new("/home/u/.local/share/stew/skills")
        );
        assert!(c.env.contains(&("N".into(), "3".into())));
        assert_eq!(c.git[1].0, "gitlab.example.com:8443");
        assert_eq!(c.channel_ports()[1].1, CHANNEL_PORT_BASE + 1);
    }

    #[test]
    fn missing_or_empty_file_is_defaults() {
        assert_eq!(parse(None, &default_instance()).unwrap(), Config::default());
        assert_eq!(ok(""), Config::default());
    }

    #[test]
    fn bento_home_requires_instance() {
        assert!(parse(None, &explicit()).is_err());
        assert!(parse(Some(""), &explicit()).is_err());
        assert!(parse(Some("disk_gib: 2"), &explicit()).is_err());
        let c = parse(
            Some("instance: {prefix: dev-, nix_port: 17761}"),
            &explicit(),
        )
        .unwrap();
        assert_eq!(c.instance.prefix, "dev-");
        assert_eq!(
            c.instance.loopback,
            Loopback::parse(DEFAULT_LOOPBACK).unwrap()
        );
    }

    #[test]
    fn unknown_keys_fail() {
        assert!(err("chanels: {}").contains("unknown key `chanels`"));
        assert!(err("instance: {prefx: a-}").contains("instance.prefx"));
        assert!(err("channels: {a: {socket: /s, mode: x}}").contains("channels.a.mode"));
        assert!(err("files: [{guest: /a, host: /b, ro: true}]").contains("files[0].ro"));
    }

    #[test]
    fn bad_values_fail() {
        for (text, want) in [
            ("instance: {nix_port: 80}", "nix_port"),
            ("instance: {nix_port: 70000}", "nix_port"),
            ("instance: {loopback: 10.0.0.0/24}", "127.0.0.0/8"),
            ("instance: {loopback: 127.77.0.5/24}", "host bits"),
            ("instance: {loopback: 127.77.0.0/31}", "8 to 30"),
            ("instance: {prefix: Bad}", "prefix"),
            ("disk_gib: 0", "disk_gib"),
            ("disk_gib: -1", "disk_gib"),
            ("channels: {a: {}}", "socket"),
            ("channels: {A: {socket: /s}}", "bad name"),
            ("channels: {a: {socket: rel}}", "absolute"),
            ("tools: {x: /home/u/flake#x}", "local path"),
            ("tools: {x: path:/a#x}", "local path"),
            ("tools: {x: ./a#x}", "local path"),
            ("tools: {x: --impure}", "flake ref"),
            ("tools: {x: 'github:a/b#x --x'}", "flake ref"),
            ("tools: {x: github:a/b}", "no package"),
            ("tools: {'../x': github:a/b#c}", "bad name"),
            ("env: {PATH: /x}", "set by bento"),
            ("env: {BENTO_X: /x}", "set by bento"),
            ("env: {'A B': x}", "not an env name"),
            ("env: {A: [1]}", "expected a string"),
            ("env_files: {HOME: /x}", "set by bento"),
            ("env: {A: x}\nenv_files: {A: /x}", "both"),
            ("git: {'a/b': /x}", "not a host"),
            ("git: {'a:0': /x}", "not a host"),
            ("files: [{guest: ../x, host: /x}]", "absolute"),
            ("files: [{guest: /a/../b, host: /x}]", ".."),
            ("files: [{guest: /a}]", "host"),
            ("files: {guest: /a}", "list"),
            ("files: [{guest: /nix/x, host: /x}]", "bento's"),
            ("files: [{guest: /bento/data/home, host: /x}]", "bento's"),
            (
                "files: [{guest: /run/bento/stew.sock, host: /x}]",
                "bento's",
            ),
            ("files: [{guest: /workspace, host: /x}]", "data disk"),
            ("files: [{guest: '~', host: /x}]", "data disk"),
            ("copy: [{guest: /, host: /x}]", "data disk"),
            ("[1, 2]", "mapping"),
            (": :", "YAML"),
        ] {
            let e = err(text);
            assert!(e.contains(want), "{text}: {e}");
        }
    }

    #[test]
    fn overlapping_guest_paths_fail() {
        for text in [
            "files: [{guest: /a, host: /x}, {guest: /a, host: /y}]",
            "files: [{guest: /a, host: /x}, {guest: /a/b, host: /y}]",
            "files: [{guest: /a/b/, host: /x}]\ncopy: [{guest: /a, host: /y}]",
            "files: [{guest: '~/.a', host: /x}]\ncopy: [{guest: /home/u/.a/b, host: /y}]",
        ] {
            assert!(err(text).contains("overlap"), "{text}");
        }
        // siblings and prefixes that aren't path prefixes are fine
        ok("files: [{guest: /a, host: /x}, {guest: /ab, host: /y}, {guest: /b/a, host: /z}]");
    }

    #[test]
    fn loopback_hosts() {
        let l = Loopback::parse("127.77.0.0/24").unwrap();
        let hosts: Vec<_> = l.hosts().collect();
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts[0], Ipv4Addr::new(127, 77, 0, 1));
        assert_eq!(hosts[253], Ipv4Addr::new(127, 77, 0, 254));
        assert_eq!(Loopback::parse("127.78.0.0/30").unwrap().hosts().count(), 2);
    }

    #[test]
    fn boot_hash_ignores_disk_size_only() {
        let a = ok(FULL);
        let mut b = a.clone();
        b.disk_gib = 1;
        assert_eq!(a.boot_hash(), b.boot_hash());
        b.env.push(("X".into(), "1".into()));
        assert_ne!(a.boot_hash(), b.boot_hash());
        let mut c = a.clone();
        c.tools[0].1 = "github:o/r/new#claude-code".into();
        assert_ne!(a.boot_hash(), c.boot_hash());
    }

    #[test]
    fn git_hosts() {
        for good in [
            "github.com",
            "gitlab.com",
            "git.example.org:8443",
            "localhost",
        ] {
            assert!(is_git_host(good), "{good}");
        }
        for bad in ["", "a/b", "a b", "-x", "a:", "a:x", "https://github.com"] {
            assert!(!is_git_host(bad), "{bad}");
        }
    }
}
