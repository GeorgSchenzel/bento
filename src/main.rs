use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitCode};

use clap::{Parser, Subcommand};
use microsandbox::image::Image;
use microsandbox::sandbox::Sandbox;
use microsandbox::setup;
use microsandbox::MicrosandboxError;
use serde_yaml::Value;

const INIT: &str = "/env/init-vibe";

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
    #[command(about = "Remove the box for this repo (stops it first)")]
    Rm,
}

struct Repo {
    dir: PathBuf,
    name: String,
    tag: String,
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
        Command::Rm => rm().await,
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
            let base = dir
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repo".into());
            let clean: String = base
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            let name = format!("bento-{clean}");
            return Ok(Repo {
                tag: format!("{name}:latest"),
                name,
                dir,
            });
        }
        if !dir.pop() {
            bail!("no devenv.nix or devenv.yaml found in this directory or any parent");
        }
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

async fn create_sandbox(repo: &Repo) -> Result<Sandbox, MicrosandboxError> {
    Sandbox::builder(repo.name.clone())
        .image(repo.tag.clone())
        .init(INIT)
        .user("0:0")
        .shell("/bin/bash")
        .detached(true)
        .replace()
        .create_detached()
        .await
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
        Err(err) => report(&err, &repo.name),
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
    let handle = match Sandbox::get(&repo.name).await {
        Ok(handle) => handle,
        Err(MicrosandboxError::SandboxNotFound(_)) => {
            println!("sandbox '{}': not found, creating...", repo.name);
            if let Some(code) = ensure_image(&repo).await {
                return code;
            }
            match create_sandbox(&repo).await {
                Ok(_) => match Sandbox::get(&repo.name).await {
                    Ok(handle) => handle,
                    Err(err) => return report(&err, &repo.name),
                },
                Err(err) => return report(&err, &repo.name),
            }
        }
        Err(err) => return report(&err, &repo.name),
    };
    let sb = match handle.connect().await {
        Ok(sb) => sb,
        Err(MicrosandboxError::SandboxNotRunning(_)) => {
            println!("sandbox '{}': not running, starting...", repo.name);
            match handle.start_detached().await {
                Ok(sb) => sb,
                Err(err) => return report(&err, &repo.name),
            }
        }
        Err(err) => return report(&err, &repo.name),
    };
    println!("entering sandbox '{}' (detach with Ctrl-])", repo.name);
    match sb.attach_shell().await {
        Ok(code) if code < 0 => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => report(&err, &repo.name),
    }
}

async fn rm() -> ExitCode {
    let repo = match discover() {
        Ok(repo) => repo,
        Err(err) => return fail(&err),
    };
    let handle = match Sandbox::get(&repo.name).await {
        Ok(handle) => handle,
        Err(MicrosandboxError::SandboxNotFound(_)) => {
            println!("sandbox '{}': not found", repo.name);
            return ExitCode::SUCCESS;
        }
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
