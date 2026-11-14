mod agent;
mod bridge;
mod builder;
mod forest;
mod gpu;
mod herdr;
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
    about = "Per-checkout and per-forest microsandbox boxes on a shared nix store"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Install the microsandbox runtime and prepare the store and builder")]
    Install,
    #[command(about = "(Re)create the box for this checkout or forest")]
    Start,
    #[command(about = "Stop the box for this checkout or forest")]
    Stop,
    #[command(about = "Open a shell in the box for this checkout or forest")]
    Enter,
    #[command(about = "Run claude in the box for this checkout or forest")]
    Claude {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(about = "Run opencode in the box for this checkout or forest")]
    Opencode {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(
        about = "Run a command in the box for this checkout or forest (no TTY needed; stdio piped, exit code passed through)"
    )]
    Exec {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    #[command(about = "Open a shell in the builder (owner of the shared nix store)")]
    Builder {
        #[command(subcommand)]
        action: Option<BuilderAction>,
    },
    #[command(about = "Remove the box for this checkout or forest and its state")]
    Rm,
    #[command(
        about = "List the builder and the boxes with their status, disk use and checkout or forest"
    )]
    Ls,
    #[command(about = "Remove the boxes whose checkout or forest is gone, and their state")]
    Prune,
    #[command(
        about = "Run the bridge daemon for the boxes: forest ops over TCP JSON lines (see README)"
    )]
    Serve {
        #[arg(long)]
        port: Option<u16>,
    },
}

#[derive(Debug, Subcommand)]
enum BuilderAction {
    #[command(about = "Recreate the builder (cuts off nix builds in progress)")]
    Restart,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // `bento serve` adopts its port as BENTO_BRIDGE_PORT (M3) so boxes it
    // creates while running (handoffs) record the reachable port. That has
    // to happen HERE, before the tokio runtime exists: set_var with worker
    // threads live is unsound (M4 — Rust makes it unsafe from 2024).
    if let Command::Serve { port } = &cli.command {
        let resolved = match port {
            Some(p) => Ok(*p),
            None => bridge::port_from_env(),
        };
        if let Err(err) = resolved.map(bridge::adopt_serve_port) {
            eprintln!("error: {err:#}");
            return ExitCode::FAILURE;
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("starting the tokio runtime");
    match runtime.block_on(run(cli.command)) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command) -> Result<u8> {
    let config = microsandbox::config::GlobalConfig::default();
    // The nix package on Linux brings its own patched runtime via MSB_PATH,
    // which sandboxes honor but `is_runtime_installed` doesn't look at.
    let packaged_runtime = std::env::var_os("MSB_PATH").is_some();
    if let Command::Install = command {
        if !packaged_runtime {
            println!("installing the microsandbox runtime...");
            setup::install_runtime(
                &config,
                setup::InstallOptions {
                    force: true, // replaces an older runtime on upgrade
                    ..Default::default()
                },
            )
            .await?;
        }
        check_host()?;
        builder::ensure().await?;
        println!("bento: ready");
        return Ok(0);
    }
    if !packaged_runtime && !setup::is_runtime_installed(&config) {
        bail!("the microsandbox runtime is not installed; run `bento install` first");
    }
    match command {
        Command::Install => unreachable!(),
        Command::Builder { action: None } => {
            need_tty("bento builder")?;
            builder::enter().await
        }
        Command::Builder {
            action: Some(BuilderAction::Restart),
        } => {
            builder::restart().await?;
            Ok(0)
        }
        Command::Start => {
            sandbox::create(&forest::discover()?).await?;
            Ok(0)
        }
        Command::Stop => {
            sandbox::stop(&forest::discover()?).await?;
            Ok(0)
        }
        Command::Rm => {
            let forest = forest::discover()?;
            // the machine entry goes before the state dir that names it
            herdr::unregister(&forest.dir, &forest.name);
            sandbox::rm(&forest).await?;
            Ok(0)
        }
        Command::Ls => {
            sandbox::ls().await?;
            Ok(0)
        }
        Command::Prune => {
            sandbox::prune().await?;
            Ok(0)
        }
        Command::Serve { .. } => {
            // main() resolved the port (--port, else BENTO_BRIDGE_PORT,
            // else the default; a bad env value is an error, not a silent
            // port) and adopted it into the env before the runtime started,
            // so boxes created by handoffs record it. Read it back.
            let port = bridge::port_from_env()?;
            bridge::serve(port).await?;
            Ok(0)
        }
        Command::Enter => {
            need_tty("bento enter")?;
            let forest = forest::discover()?;
            let sb = sandbox::ensure_running(&forest).await?;
            println!("entering box '{}' (detach with Ctrl-])", forest.name);
            util::attach(&sb, "bash", &[], &forest.cwd, util::term_env()).await
        }
        Command::Claude { args } => run_agent(Agent::Claude, &args).await,
        Command::Opencode { args } => run_agent(Agent::Opencode, &args).await,
        Command::Exec { args } => {
            let forest = forest::discover()?;
            let sb = sandbox::ensure_running(&forest).await?;
            util::exec(&sb, &args[0], &args[1..], &forest.cwd).await
        }
    }
}

async fn run_agent(agent: Agent, args: &[String]) -> Result<u8> {
    need_tty(&format!("bento {}", agent.command()))?;
    let forest = forest::discover()?;
    // fail on a missing token before creating anything
    let mut env = agent.env(&util::home()?)?;
    let sb = sandbox::ensure_running(&forest).await?;
    if let Agent::Opencode = agent {
        let state = sandbox::state_dir(&forest)?;
        if let Err(e) = agent::import_opencode_credentials(&sb, &util::home()?, &state).await {
            eprintln!("bento: importing opencode credentials: {e:#}");
        }
    }
    env.extend(util::term_env());
    util::attach(
        &sb,
        agent.command(),
        &agent.launch_args(args),
        &forest.cwd,
        env,
    )
    .await
}

/// Fail on what stops VMs from booting (on Linux: no /dev/kvm or no access to
/// it) with msb's own hints, rather than on the first sandbox.
fn check_host() -> Result<()> {
    let problems: Vec<String> = setup::diagnose()
        .problems
        .iter()
        .map(|p| format!("{}\n  {}", p.headline, p.hints.join("\n  ")))
        .collect();
    if !problems.is_empty() {
        bail!("this host can't run boxes:\n{}", problems.join("\n"));
    }
    Ok(())
}

fn need_tty(what: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("stdin is not a terminal; `{what}` needs an interactive terminal");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn exec_args(argv: &[&str]) -> Vec<String> {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Exec { args } => args,
            other => panic!("not an exec command: {other:?}"),
        }
    }

    #[test]
    fn exec_takes_a_command_with_and_without_the_separator() {
        // `--` separates bento's flags from the command's; without it the
        // first word is the command and the rest is passed raw
        assert_eq!(
            exec_args(&["bento", "exec", "--", "cat", "-v"]),
            ["cat", "-v"]
        );
        assert_eq!(exec_args(&["bento", "exec", "cat", "-v"]), ["cat", "-v"]);
        assert_eq!(
            exec_args(&["bento", "exec", "--", "sh", "-c", "exit 3"]),
            ["sh", "-c", "exit 3"]
        );
        assert_eq!(exec_args(&["bento", "exec", "--", "--weird"]), ["--weird"]);
    }

    #[test]
    fn exec_without_a_command_is_a_usage_error() {
        for argv in [
            ["bento", "exec"].as_slice(),
            ["bento", "exec", "--"].as_slice(),
        ] {
            let err = Cli::try_parse_from(argv).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
        }
    }
}
