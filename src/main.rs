use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use microsandbox::sandbox::{PullProgress, Sandbox};
use microsandbox::setup;
use microsandbox::MicrosandboxError;

const NAME: &str = "alpine";
const IMAGE: &str = "alpine";

#[derive(Debug, Parser)]
#[command(name = "bento", about = "Manage a microsandbox alpine box")]
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
    #[command(about = "Start the alpine sandbox (creates it on first run)")]
    Start,
    #[command(about = "Stop the alpine sandbox")]
    Stop,
    #[command(about = "Open an interactive shell in the alpine sandbox")]
    Enter,
    #[command(about = "Remove the alpine sandbox (stops it first)")]
    Rm,
}

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Check => check(),
        Command::Install => install().await,
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
        Err(err) => report(&err),
    }
}

fn ensure_runtime() -> Option<ExitCode> {
    if setup::is_installed() {
        return None;
    }
    eprintln!("error: microsandbox runtime is not installed; run `bento install` first");
    Some(ExitCode::FAILURE)
}

async fn start() -> ExitCode {
    if let Some(code) = ensure_runtime() {
        return code;
    }
    create_first().await
}

async fn create_first() -> ExitCode {
    let (mut progress, task) = match Sandbox::builder(NAME)
        .image(IMAGE)
        .detached(true)
        .replace()
        .create_detached_with_pull_progress()
    {
        Ok(pair) => pair,
        Err(err) => return report(&err),
    };
    while let Some(event) = progress.recv().await {
        if let PullProgress::Resolved { layer_count, .. } = event {
            println!("pulling {IMAGE}: {layer_count} layer(s)");
        }
    }
    match task.await {
        Ok(Ok(_)) => {
            println!("sandbox '{NAME}': created and started");
            ExitCode::SUCCESS
        }
        Ok(Err(err)) => report(&err),
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn stop() -> ExitCode {
    let handle = match Sandbox::get(NAME).await {
        Ok(handle) => handle,
        Err(err) => return report(&err),
    };
    match handle.stop().await {
        Ok(()) => {
            println!("sandbox '{NAME}': stopped");
            ExitCode::SUCCESS
        }
        Err(err) => report(&err),
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
    let handle = match Sandbox::get(NAME).await {
        Ok(handle) => handle,
        Err(MicrosandboxError::SandboxNotFound(_)) => {
            println!("sandbox '{NAME}': not found, creating...");
            match create_first().await {
                ExitCode::SUCCESS => (),
                code => return code,
            }
            match Sandbox::get(NAME).await {
                Ok(handle) => handle,
                Err(err) => return report(&err),
            }
        }
        Err(err) => return report(&err),
    };
    let sb = match handle.connect().await {
        Ok(sb) => sb,
        Err(MicrosandboxError::SandboxNotRunning(_)) => {
            println!("sandbox '{NAME}': not running, starting...");
            match handle.start_detached().await {
                Ok(sb) => sb,
                Err(err) => return report(&err),
            }
        }
        Err(err) => return report(&err),
    };
    println!("entering sandbox '{NAME}' (detach with Ctrl-])");
    match sb.attach_shell().await {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => report(&err),
    }
}

async fn rm() -> ExitCode {
    let handle = match Sandbox::get(NAME).await {
        Ok(handle) => handle,
        Err(MicrosandboxError::SandboxNotFound(_)) => {
            println!("sandbox '{NAME}': not found");
            return ExitCode::SUCCESS;
        }
        Err(err) => return report(&err),
    };
    if let Err(err) = handle.stop().await {
        return report(&err);
    }
    match handle.remove().await {
        Ok(()) => {
            println!("sandbox '{NAME}': removed");
            ExitCode::SUCCESS
        }
        Err(err) => report(&err),
    }
}

fn report(err: &MicrosandboxError) -> ExitCode {
    match err {
        MicrosandboxError::SandboxNotFound(_) => {
            eprintln!("error: no sandbox named '{NAME}'");
        }
        MicrosandboxError::SandboxNotRunning(_) => {
            eprintln!("error: sandbox '{NAME}' is not running; run `bento start` first");
        }
        _ => eprintln!("error: {err}"),
    }
    ExitCode::FAILURE
}
