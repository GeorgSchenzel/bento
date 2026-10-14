mod agent;
mod builder;
mod repo;
mod sandbox;
mod store;
mod util;

use std::io::IsTerminal as _;
use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use microsandbox::setup;

use agent::Agent;

#[derive(Debug, Parser)]
#[command(
    name = "bento",
    about = "Per-checkout microsandbox boxes on a shared nix store"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Install the microsandbox runtime and prepare the store and builder")]
    Install,
    #[command(about = "(Re)create the box for this checkout")]
    Start,
    #[command(about = "Stop the box for this checkout")]
    Stop,
    #[command(about = "Open a shell in the box for this checkout")]
    Enter,
    #[command(about = "Run claude in the box for this checkout")]
    Claude {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(about = "Run opencode in the box for this checkout")]
    Opencode {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(about = "Open a shell in the builder (owner of the shared nix store)")]
    Builder,
    #[command(about = "Remove the box for this checkout and its state")]
    Clean,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse().command).await {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command) -> Result<u8> {
    let config = microsandbox::config::GlobalConfig::default();
    if let Command::Install = command {
        println!("installing the microsandbox runtime...");
        setup::install_runtime(
            &config,
            setup::InstallOptions {
                force: true, // replaces an older runtime on upgrade
                ..Default::default()
            },
        )
        .await?;
        builder::ensure().await?;
        println!("bento: ready");
        return Ok(0);
    }
    if !setup::is_runtime_installed(&config) {
        bail!("the microsandbox runtime is not installed; run `bento install` first");
    }
    match command {
        Command::Install => unreachable!(),
        Command::Builder => {
            need_tty("bento builder")?;
            builder::enter().await
        }
        Command::Start => {
            sandbox::create(&repo::discover()?).await?;
            Ok(0)
        }
        Command::Stop => {
            sandbox::stop(&repo::discover()?).await?;
            Ok(0)
        }
        Command::Clean => {
            sandbox::clean(&repo::discover()?).await?;
            Ok(0)
        }
        Command::Enter => {
            need_tty("bento enter")?;
            let repo = repo::discover()?;
            let sb = sandbox::ensure_running(&repo).await?;
            println!("entering box '{}' (detach with Ctrl-])", repo.name);
            util::attach(&sb, "bash", &[], sandbox::WORKDIR, util::term_env()).await
        }
        Command::Claude { args } => run_agent(Agent::Claude, &args).await,
        Command::Opencode { args } => run_agent(Agent::Opencode, &args).await,
    }
}

async fn run_agent(agent: Agent, args: &[String]) -> Result<u8> {
    need_tty(&format!("bento {}", agent.command()))?;
    let repo = repo::discover()?;
    // fail on a missing token before creating anything
    let mut env = agent.env(&util::home()?)?;
    let sb = sandbox::ensure_running(&repo).await?;
    env.extend(util::term_env());
    util::attach(&sb, agent.command(), args, sandbox::WORKDIR, env).await
}

fn need_tty(what: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("stdin is not a terminal; `{what}` needs an interactive terminal");
    }
    Ok(())
}
