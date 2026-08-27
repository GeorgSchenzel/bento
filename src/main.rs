use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitCode};

use clap::{Parser, Subcommand};
use microsandbox::image::Image;
use microsandbox::sandbox::Sandbox;
use microsandbox::setup;
use microsandbox::Volume;
use microsandbox::MicrosandboxError;
use serde_yaml::Value;

const INIT: &str = "/env/init-vibe";
const GUEST_HOME: &str = "/env";
const WORKDIR: &str = "/workspace";
/// Linked-worktree git dir inside the box: the main checkout's .git is bound
/// here rw (commits write objects/refs/index through it).
const GUEST_GIT: &str = "/git";

const DEFAULT_YAML: &str = "\
inputs:
  nixpkgs:
    url: github:cachix/devenv-nixpkgs/rolling
  mk-shell-bin:
    url: github:rrbutani/nix-mk-shell-bin
  nix2container:
    url: github:nlewo/nix2container
    inputs:
      nixpkgs:
        follows: nixpkgs
";

const BOX_NIX: &str = r#"{ pkgs, lib, config, ... }:

{
  # bento-added tooling: the coding agent available in every box. git is
  # required for in-box commits even when the repo's own devenv omits it.
  packages = [ pkgs.opencode pkgs.git ];

  # PID 1 in the box is procfileScript via the init shim; with no services
  # devenv-tasks exits at once and takes the VM down. keepalive holds the
  # box open (also required for evaluation: CLI 2.0 nulls task.package
  # when no processes are defined, breaking the image build).
  processes = lib.mkIf config.containers.processes.isBuilding {
    keepalive.exec = "sleep infinity";
  };

  # msb never executes the OCI Entrypoint/Cmd (its agent stays PID 1); only
  # `--init <path>` hands PID 1 over. This generic shim execs the devenv
  # entrypoint with the image Cmd — exactly what `docker run` does.
  # copyToRoot lands under the container home /env, not /, so the shim path
  # is /env/init-vibe.
  containers.processes.copyToRoot = [
    (pkgs.writeTextFile {
      name = "init-vibe";
      destination = "/init-vibe";
      executable = true;
      text = ''
        #!/bin/bash
        exec ${lib.head config.containers.processes.entrypoint} ${config.procfileScript}
      '';
    })
  ];
}
"#;

#[derive(Debug, Parser)]
#[command(name = "bento", about = "Run a repo's devenv inside a microsandbox box")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Check whether the microsandbox runtime is installed")]
    Check,
    #[command(about = "Install the microsandbox runtime if missing")]
    Install,
    #[command(about = "Build the devenv container image for this repo")]
    Build,
    #[command(about = "Start the box for this repo (builds the image when missing)")]
    Start,
    #[command(about = "Stop the box for this repo")]
    Stop,
    #[command(about = "Open an interactive shell in the box for this repo")]
    Enter,
    #[command(about = "Run opencode in the box for this repo")]
    Agent,
    #[command(about = "Remove the box and its opencode volume for this repo")]
    Clean,
}

/// A linked worktree: its .git is a file pointing into the main checkout's
/// .git/worktrees/<id>, which lives outside the worktree bind.
struct WorktreeGit {
    main_git: PathBuf,
    wt_id: String,
}

struct Repo {
    dir: PathBuf,
    name: String,
    tag: String,
    wt: Option<WorktreeGit>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Check => check(),
        Command::Install => install().await,
        Command::Build => build_cmd().await,
        Command::Start => start().await,
        Command::Stop => stop().await,
        Command::Enter => enter().await,
        Command::Agent => agent().await,
        Command::Clean => clean().await,
    }
}

fn check() -> ExitCode {
    if setup::is_installed() {
        println!("microsandbox runtime: installed");
        ExitCode::SUCCESS
    } else {
        println!("microsandbox runtime: not installed");
        println!("run `bento install` to set it up");
        ExitCode::FAILURE
    }
}

async fn install() -> ExitCode {
    if setup::is_installed() {
        println!("microsandbox runtime: already installed");
        return ExitCode::SUCCESS;
    }
    println!("installing microsandbox runtime...");
    match setup::install().await {
        Ok(()) => {
            println!("microsandbox runtime: installed");
            ExitCode::SUCCESS
        }
        Err(err) => report(&err, ""),
    }
}

fn ensure_runtime() -> Option<ExitCode> {
    if setup::is_installed() {
        return None;
    }
    eprintln!("error: microsandbox runtime is not installed; run `bento install` first");
    Some(ExitCode::FAILURE)
}

use anyhow::{bail, Context as _};

fn discover() -> anyhow::Result<Repo> {
    let mut dir = std::env::current_dir().context("cannot determine working directory")?;
    loop {
        if dir.join("devenv.nix").is_file() || dir.join("devenv.yaml").is_file() {
            return Ok(repo_at(dir));
        }
        if !dir.pop() {
            bail!("no devenv.nix or devenv.yaml found in this directory or any parent");
        }
    }
}

/// Derive sandbox/image names for a checkout. Sandboxes are one per checkout
/// (`bento_{project}` for the main one, `bento_{project}_{dir}` for
/// worktrees); the image is shared per project. Everything is collision-safe
/// via the bento.path label guard in [create_sandbox], not via hashes.
fn repo_at(dir: PathBuf) -> Repo {
    let wt = detect_worktree(&dir);
    let project = git(&dir, &["config", "--get", "remote.origin.url"])
        .map(|url| remote_project(&url))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| slug(&dir_basename(&dir)));
    let checkout = if wt.is_some() {
        format!("bento_{project}_{}", slug(&dir_basename(&dir)))
    } else {
        format!("bento_{project}")
    };
    // microsandbox caps sandbox names at 128 bytes and requires an ascii
    // alphanumeric start (always satisfied by the "bento_" prefix).
    let name = truncate(&checkout, 128).to_string();
    let image = truncate(&format!("bento_{project}"), 128).to_string();
    Repo {
        tag: format!("{image}:latest"),
        name,
        dir,
        wt,
    }
}

fn dir_basename(dir: &Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
}

/// Project slug from a remote URL: the path's last segment sans .git.
fn remote_project(url: &str) -> String {
    let base = url.rsplit('/').next().unwrap_or(url);
    slug(base.strip_suffix(".git").unwrap_or(base))
}

/// microsandbox names allow only [A-Za-z0-9._-] and must start alphanumeric.
fn slug(input: &str) -> String {
    let s: String = input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string();
    if s.is_empty() {
        return "repo".into();
    }
    s
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Process::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Detect a linked worktree. Its .git is a file containing a `gitdir:`
/// pointer into the main checkout's .git/worktrees/<id>. Submodules use the
/// same file format but point into modules/ — not worktrees.
fn detect_worktree(dir: &Path) -> Option<WorktreeGit> {
    let git_file = dir.join(".git");
    if !git_file.is_file() {
        return None;
    }
    let text = fs::read_to_string(git_file).ok()?;
    let line = text.lines().find(|l| l.starts_with("gitdir:"))?;
    let raw = Path::new(line["gitdir:".len()..].trim());
    let pointer = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        dir.join(raw)
    };
    let wt_git = fs::canonicalize(pointer).ok()?;
    if wt_git.parent()?.file_name()? != "worktrees" {
        return None;
    }
    let wt_id = wt_git.file_name()?.to_string_lossy().into_owned();
    let main_git = fs::canonicalize(wt_git.join("../..")).ok()?;
    Some(WorktreeGit { main_git, wt_id })
}

/// Host-side generated files (gitconfig, gitdir pointers) for one sandbox.
/// Kept outside any mounted dir: file binds are re-staged at every start, so
/// the host files must persist independently.
fn state_dir(name: &str) -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let dir = PathBuf::from(home).join(".local/state/bento").join(name);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// In-box commit identity, resolved the way host git would (local then
/// global config).
fn git_identity(dir: &Path) -> anyhow::Result<(String, String)> {
    match (
        git(dir, &["config", "--get", "user.name"]),
        git(dir, &["config", "--get", "user.email"]),
    ) {
        (Some(name), Some(email)) => Ok((name, email)),
        _ => bail!(
            "git identity is not configured; set it on the host with:\n  git config --global user.name <name>\n  git config --global user.email <email>"
        ),
    }
}

fn materialize_wrapper(repo: &Repo, wrapper: &Path) -> anyhow::Result<()> {
    let mut doc = read_yaml_base(repo)?;
    let Value::Mapping(map) = &mut doc else {
        bail!("target devenv.yaml is not a mapping");
    };
    if let Some(Value::Sequence(entries)) = map.remove(Value::String("imports".into())) {
        if !entries.is_empty() {
            let dropped: Vec<&str> = entries
                .iter()
                .map(|e| e.as_str().unwrap_or("<non-string>"))
                .collect();
            eprintln!(
                "warning: ignoring {} import(s) from {} (bento does not yet support them): {}",
                dropped.len(),
                repo.dir.join("devenv.yaml").display(),
                dropped.join(", ")
            );
        }
    }
    let mut inputs_map = match map.remove(Value::String("inputs".into())) {
        Some(Value::Mapping(m)) => m,
        _ => serde_yaml::Mapping::new(),
    };
    let defaults = match serde_yaml::from_str::<Value>(DEFAULT_YAML) {
        Ok(Value::Mapping(m)) => m,
        _ => serde_yaml::Mapping::new(),
    };
    if let Some(Value::Mapping(default_inputs)) =
        defaults.get(Value::String("inputs".into())).cloned()
    {
        for (key, value) in default_inputs {
            inputs_map.entry(key).or_insert(value);
        }
    }
    let mut target = serde_yaml::Mapping::new();
    target.insert(
        Value::String("url".into()),
        Value::String(format!("path:{}", repo.dir.display())),
    );
    target.insert(Value::String("flake".into()), Value::Bool(false));
    inputs_map.insert(Value::String("bento-target".into()), Value::Mapping(target));
    map.insert(Value::String("inputs".into()), Value::Mapping(inputs_map));
    map.insert(
        Value::String("imports".into()),
        Value::Sequence(vec![Value::String("bento-target".into())]),
    );
    let yaml = serde_yaml::to_string(&doc).context("serializing wrapper devenv.yaml")?;
    fs::write(wrapper.join("devenv.yaml"), &yaml)
        .with_context(|| format!("writing {}", wrapper.join("devenv.yaml").display()))?;
    fs::write(wrapper.join("devenv.nix"), BOX_NIX)
        .with_context(|| format!("writing {}", wrapper.join("devenv.nix").display()))?;
    let lock = repo.dir.join("devenv.lock");
    if lock.is_file() {
        fs::copy(&lock, wrapper.join("devenv.lock"))
            .with_context(|| format!("copying {}", lock.display()))?;
    }
    Ok(())
}

fn read_yaml_base(repo: &Repo) -> anyhow::Result<Value> {
    let path = repo.dir.join("devenv.yaml");
    if !path.is_file() {
        return Ok(serde_yaml::from_str(DEFAULT_YAML)
            .context("parsing built-in devenv.yaml template")?);
    }
    let text =
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(serde_yaml::from_str(DEFAULT_YAML)
            .context("parsing built-in devenv.yaml template")?);
    }
    serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

async fn build(repo: &Repo) -> anyhow::Result<()> {
    let wrapper = tempfile::tempdir().context("creating temp dir")?;
    materialize_wrapper(repo, wrapper.path())?;
    let tar = wrapper.path().join("box.tar");
    let registry = format!("oci-archive:{}:", tar.display());
    let status = Process::new("devenv")
        .args(["container", "copy", "processes", "--registry", &registry])
        .current_dir(wrapper.path())
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("devenv not found on PATH")
            } else {
                anyhow::anyhow!("failed to run devenv: {e}")
            }
        })?;
    if !status.success() {
        bail!(
            "devenv container copy failed with exit code {}",
            status.code().unwrap_or(-1)
        );
    }
    Image::load(&tar, vec![repo.tag.clone()])
        .await
        .context("loading image into microsandbox")?;
    println!("image '{}': built", repo.tag);
    Ok(())
}

async fn ensure_image(repo: &Repo) -> Option<ExitCode> {
    if Image::get(&repo.tag).await.is_ok() {
        return None;
    }
    println!("image '{}' not found, building...", repo.tag);
    match build(repo).await {
        Ok(()) => None,
        Err(err) => Some(fail(&err)),
    }
}

/// Host terminal identity for attach sessions. The SDK does not propagate
/// TERM itself; TUIs (opencode) and colored shell output need it.
fn term_env() -> Vec<(String, String)> {
    let mut env = vec![(
        "TERM".to_string(),
        std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
    )];
    if let Ok(colorterm) = std::env::var("COLORTERM") {
        env.push(("COLORTERM".to_string(), colorterm));
    }
    env
}

fn home_file(rel: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(rel);
    match fs::canonicalize(&path) {
        // Resolve symlinks (sops-rendered secrets, dotfile managers) so the
        // host-side bind serves the real file.
        Ok(canonical) => Some(canonical),
        Err(_) => {
            eprintln!("warning: ~/{rel} not found, skipping its mount");
            None
        }
    }
}

async fn create_sandbox(repo: &Repo) -> anyhow::Result<Sandbox> {
    // Canonicalize so symlinked paths (managed dotdirs, /tmp on macOS) bind
    // the real host target.
    let project = fs::canonicalize(&repo.dir).unwrap_or_else(|_| repo.dir.clone());
    let project_str = project.display().to_string();

    // Names are path-derived; if another checkout ever maps to the same
    // name, refuse to clobber its sandbox instead of silently --replace-ing.
    if let Ok(handle) = Sandbox::get(&repo.name).await {
        if let Some(owner) = handle
            .config()
            .ok()
            .and_then(|c| c.spec.labels.get("bento.path").cloned())
        {
            if owner != project_str {
                bail!(
                    "sandbox '{}' already belongs to {owner}; refusing to replace it",
                    repo.name
                );
            }
        }
    }

    let state = state_dir(&repo.name)?;
    let gitconfig = write_gitconfig(repo, &state)?;

    let opencode_state = format!("{GUEST_HOME}/.local/share/opencode");
    let mut builder = Sandbox::builder(repo.name.clone())
        .image(repo.tag.clone())
        .init(INIT)
        .user("0:0")
        .shell("/bin/bash")
        .cpus(4)
        .memory(8192)
        .workdir(WORKDIR)
        .detached(true)
        .replace()
        .label("bento.path", &project_str)
        .volume(WORKDIR, |m| m.bind(&project))
        .volume(format!("{GUEST_HOME}/.gitconfig"), |m| {
            m.bind(&gitconfig).readonly()
        })
        .volume(opencode_state.clone(), |m| {
            m.named_with(format!("{}-opencode", repo.name), |v| v.ensure_exists())
        });
    if let Some(wt) = &repo.wt {
        let (pointer, back) = write_worktree_pointers(wt, &state)?;
        // The main checkout's git DB, mounted rw: in-box commits write
        // objects/refs/index through it, landing directly on the host.
        builder = builder.volume(GUEST_GIT, |m| m.bind(&wt.main_git));
        // Shadow the host .git file (whose gitdir points to a host-only
        // path) with one pointing into the box, and repoint the worktree
        // entry's gitdir back at /workspace/.git so both directions resolve.
        builder = builder
            .volume(format!("{WORKDIR}/.git"), |m| m.bind(&pointer).readonly())
            .volume(
                format!("{GUEST_GIT}/worktrees/{}/gitdir", wt.wt_id),
                |m| m.bind(&back).readonly(),
            );
    }
    if let Some(auth) = home_file(".local/share/opencode/auth.json") {
        // Nested ro file bind over the volume: the token stays on the host
        // and never lands in the volume or the image.
        builder = builder.volume(format!("{opencode_state}/auth.json"), |m| {
            m.bind(auth).readonly()
        });
    }
    if let Some(config) = home_file(".config/opencode/opencode.json") {
        builder = builder.volume(format!("{GUEST_HOME}/.config/opencode/opencode.json"), |m| {
            m.bind(config).readonly()
        });
    }
    builder
        .create_detached()
        .await
        .with_context(|| format!("creating sandbox '{}'", repo.name))
}

/// Minimal in-box git identity: user from host config, no signing (host
/// signing keys/helpers are unavailable in the box), and safe.directory for
/// the bind-mounted worktree and git dir.
fn write_gitconfig(repo: &Repo, state: &Path) -> anyhow::Result<PathBuf> {
    let (name, email) = git_identity(&repo.dir)?;
    let mut text = format!(
        "[user]\n\tname = {name}\n\temail = {email}\n\
         [commit]\n\tgpgsign = false\n\
         [tag]\n\tgpgsign = false\n\
         [safe]\n\tdirectory = {WORKDIR}\n\tdirectory = {GUEST_GIT}\n"
    );
    // git also checks the resolved gitdir for dubious ownership
    if repo.wt.is_some() {
        text.push_str(&format!("\tdirectory = {GUEST_GIT}/worktrees\n"));
    }
    let path = state.join("gitconfig");
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Guest-side gitdir pointers for a linked worktree. On the host, the
/// worktree's .git file and the worktrees/<id>/gitdir back-pointer contain
/// host paths; inside the box they must reference /workspace and /git.
fn write_worktree_pointers(wt: &WorktreeGit, state: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let pointer = state.join("gitdir-pointer");
    fs::write(&pointer, format!("gitdir: {GUEST_GIT}/worktrees/{}\n", wt.wt_id))
        .with_context(|| format!("writing {}", pointer.display()))?;
    let back = state.join("gitdir-back");
    fs::write(&back, format!("{WORKDIR}/.git\n"))
        .with_context(|| format!("writing {}", back.display()))?;
    Ok((pointer, back))
}

/// Resolve the repo's box to a connected running sandbox, creating or
/// starting it as needed. Prints diagnostics; Err carries the exit code.
async fn ensure_running(repo: &Repo) -> Result<Sandbox, ExitCode> {
    let handle = match Sandbox::get(&repo.name).await {
        Ok(handle) => handle,
        Err(MicrosandboxError::SandboxNotFound(_)) => {
            println!("sandbox '{}': not found, creating...", repo.name);
            if let Some(code) = ensure_image(repo).await {
                return Err(code);
            }
            match create_sandbox(repo).await {
                Ok(_) => match Sandbox::get(&repo.name).await {
                    Ok(handle) => handle,
                    Err(err) => return Err(report(&err, &repo.name)),
                },
                Err(err) => return Err(fail(&err)),
            }
        }
        Err(err) => return Err(report(&err, &repo.name)),
    };
    match handle.connect().await {
        Ok(sb) => Ok(sb),
        Err(MicrosandboxError::SandboxNotRunning(_)) => {
            println!("sandbox '{}': not running, starting...", repo.name);
            handle.start_detached().await.map_err(|err| report(&err, &repo.name))
        }
        Err(err) => Err(report(&err, &repo.name)),
    }
}

async fn build_cmd() -> ExitCode {
    let repo = match discover() {
        Ok(repo) => repo,
        Err(err) => return fail(&err),
    };
    match build(&repo).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail(&err),
    }
}

async fn start() -> ExitCode {
    if let Some(code) = ensure_runtime() {
        return code;
    }
    let repo = match discover() {
        Ok(repo) => repo,
        Err(err) => return fail(&err),
    };
    if let Some(code) = ensure_image(&repo).await {
        return code;
    }
    match create_sandbox(&repo).await {
        Ok(_) => {
            println!("sandbox '{}': created and started", repo.name);
            ExitCode::SUCCESS
        }
        Err(err) => fail(&err),
    }
}

async fn stop() -> ExitCode {
    let repo = match discover() {
        Ok(repo) => repo,
        Err(err) => return fail(&err),
    };
    let handle = match Sandbox::get(&repo.name).await {
        Ok(handle) => handle,
        Err(err) => return report(&err, &repo.name),
    };
    match handle.stop().await {
        Ok(()) => {
            println!("sandbox '{}': stopped", repo.name);
            ExitCode::SUCCESS
        }
        Err(err) => report(&err, &repo.name),
    }
}

async fn enter() -> ExitCode {
    if !std::io::stdin().is_terminal() {
        eprintln!("error: stdin is not a terminal; `bento enter` needs an interactive terminal");
        return ExitCode::FAILURE;
    }
    if let Some(code) = ensure_runtime() {
        return code;
    }
    let repo = match discover() {
        Ok(repo) => repo,
        Err(err) => return fail(&err),
    };
    let sb = match ensure_running(&repo).await {
        Ok(sb) => sb,
        Err(code) => return code,
    };
    println!("entering sandbox '{}' (detach with Ctrl-])", repo.name);
    match sb
        .attach_with("/bin/bash", |a| {
            a.cwd(WORKDIR).envs(term_env())
        })
        .await
    {
        Ok(code) if code < 0 => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => report(&err, &repo.name),
    }
}

async fn agent() -> ExitCode {
    if !std::io::stdin().is_terminal() {
        eprintln!("error: stdin is not a terminal; `bento agent` needs an interactive terminal");
        return ExitCode::FAILURE;
    }
    if let Some(code) = ensure_runtime() {
        return code;
    }
    let repo = match discover() {
        Ok(repo) => repo,
        Err(err) => return fail(&err),
    };
    let sb = match ensure_running(&repo).await {
        Ok(sb) => sb,
        Err(code) => return code,
    };
    println!("starting opencode in sandbox '{}' (detach with Ctrl-])", repo.name);
    match sb
        .attach_with("opencode", |a| {
            a.cwd(WORKDIR).envs(term_env())
        })
        .await
    {
        Ok(code) if code < 0 => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => report(&err, &repo.name),
    }
}

async fn clean() -> ExitCode {
    let repo = match discover() {
        Ok(repo) => repo,
        Err(err) => return fail(&err),
    };
    let code = rm_sandbox(&repo).await;
    if code != ExitCode::SUCCESS {
        return code;
    }
    let volume = format!("{}-opencode", repo.name);
    match Volume::remove(&volume).await {
        Ok(()) => {
            println!("volume '{volume}': removed");
        }
        Err(MicrosandboxError::VolumeNotFound(_)) => {}
        Err(err) => return report(&err, &repo.name),
    }
    // Generated gitconfig/pointers (the image is shared per project, keep it).
    if let Some(home) = std::env::var_os("HOME") {
        let state = PathBuf::from(home)
            .join(".local/state/bento")
            .join(&repo.name);
        match fs::remove_dir_all(&state) {
            Ok(()) => println!("state '{}': removed", state.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!("error: removing {}: {e}", state.display());
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

async fn rm_sandbox(repo: &Repo) -> ExitCode {
    let handle = match Sandbox::get(&repo.name).await {
        Ok(handle) => handle,
        Err(MicrosandboxError::SandboxNotFound(_)) => return ExitCode::SUCCESS,
        Err(err) => return report(&err, &repo.name),
    };
    if let Err(err) = handle.stop().await {
        return report(&err, &repo.name);
    }
    match handle.remove().await {
        Ok(()) => {
            println!("sandbox '{}': removed", repo.name);
            ExitCode::SUCCESS
        }
        Err(err) => report(&err, &repo.name),
    }
}

fn fail(err: &anyhow::Error) -> ExitCode {
    eprintln!("error: {err:#}");
    ExitCode::FAILURE
}

fn report(err: &MicrosandboxError, name: &str) -> ExitCode {
    match err {
        MicrosandboxError::SandboxNotFound(_) => {
            eprintln!("error: no sandbox named '{name}'");
        }
        MicrosandboxError::SandboxNotRunning(_) => {
            eprintln!("error: sandbox '{name}' is not running");
        }
        _ => eprintln!("error: {err}"),
    }
    ExitCode::FAILURE
}
