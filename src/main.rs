mod boxes;
mod builder;
mod config;
mod cp;
mod exec;
mod fail;
mod guest;
mod instance;
mod names;
mod state;
mod store;
mod util;
mod workspace;

use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};

use instance::Instance;

/// The interface version (contracts.md).
pub const CONTRACT: u32 = 2;

#[derive(Debug, Parser)]
#[command(
    name = "bento",
    about = "Local VM sandboxes for agents: a box with its own disk, per-box ports, channels to host sockets, a shared nix store",
    version = concat!(env!("CARGO_PKG_VERSION"), " (contract 2)"),
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Make a box and boot it once
    Create {
        name: String,
        /// Per-box env, for the box's life
        #[arg(short = 'e', value_name = "K=V")]
        env: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Boot a stopped box (a running one: warn about what waits for its next boot)
    Start {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Stop, then start: applies pending ports and a changed config
    Restart {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Stop a box (its processes get SIGHUP, then SIGKILL after 5 s)
    Stop { name: String },
    /// Remove a box, its data disk and its state
    Rm {
        name: String,
        /// Also for broken states; a missing box is fine
        #[arg(long)]
        force: bool,
    },
    /// Run a command in a box (argv, no shell)
    Exec {
        name: String,
        /// Fail with 122 instead of booting a stopped box
        #[arg(long)]
        no_start: bool,
        /// Forward stdin
        #[arg(short = 'i')]
        interactive: bool,
        /// A pty (needs a terminal)
        #[arg(short = 't')]
        tty: bool,
        /// Working dir (absolute; default /workspace)
        #[arg(short = 'w', value_name = "DIR")]
        workdir: Option<String>,
        /// K=V, or K alone: K's value from this environment (unset: not set)
        #[arg(short = 'e', value_name = "K[=V]")]
        env: Vec<String>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// bash in a box
    Enter {
        name: String,
        #[arg(long)]
        no_start: bool,
        #[arg(short = 'w', value_name = "DIR")]
        workdir: Option<String>,
        /// K=V, or K alone: K's value from this environment (unset: not set)
        #[arg(short = 'e', value_name = "K[=V]")]
        env: Vec<String>,
    },
    /// Copy a file or dir into or out of a box (`<name>:<path>` is the box side)
    Cp { src: String, dest: String },
    /// List this instance's boxes
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// A box's declared ports and where the host reaches them
    Ports {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// `exec <name> -i -t -- claude …`
    Claude {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// `exec <name> -i -t -- opencode …`
    Opencode {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Prepare the store and the builder (operator command)
    Install,
    /// A shell in the builder, or `restart` it (operator command)
    Builder {
        #[command(subcommand)]
        action: Option<BuilderAction>,
    },
}

#[derive(Debug, Subcommand)]
enum BuilderAction {
    /// Recreate the builder (cuts off nix builds in progress)
    Restart,
    /// Remove the builder (the store stays; the next boot recreates it)
    Rm,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            use clap::error::ErrorKind;
            let code = match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
                _ => fail::USAGE,
            };
            let _ = e.print();
            return ExitCode::from(code);
        }
    };
    let instance = match setup_instance() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("bento: {e:#}");
            return ExitCode::from(fail::OTHER);
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("bento: {e}");
            return ExitCode::from(fail::OTHER);
        }
    };
    match rt.block_on(run(&instance, cli.command)) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("bento: {err:#}");
            ExitCode::from(fail::code_of(&err))
        }
    }
}

/// Resolve the instance and point msb at its home, before any thread starts
/// and before the SDK is touched: an inherited MSB_HOME is replaced, so a
/// dev instance never reaches another instance's boxes.
fn setup_instance() -> Result<Instance> {
    let instance = Instance::resolve(
        std::env::var_os("BENTO_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )?;
    instance.check_msb_home()?;
    std::fs::create_dir_all(instance.msb_home())
        .with_context(|| format!("creating {}", instance.msb_home().display()))?;
    // SAFETY: single-threaded here (the runtime starts after this)
    std::env::set_var("MSB_HOME", instance.msb_home());
    Ok(instance)
}

fn print_json(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string(v)?);
    Ok(())
}

async fn run(instance: &Instance, command: Command) -> Result<u8> {
    match command {
        Command::Create { name, env, json } => {
            names::check_box(&name)?;
            let env = env
                .iter()
                .map(|e| names::parse_env_arg(e))
                .collect::<Result<Vec<_>>>()?;
            let st = boxes::create(instance, &name, env).await?;
            box_out(instance, &st, json).await
        }
        Command::Start { name, json } => {
            let st = boxes::start(instance, &name).await?;
            box_out(instance, &st, json).await
        }
        Command::Restart { name, json } => {
            let st = boxes::restart(instance, &name).await?;
            box_out(instance, &st, json).await
        }
        Command::Stop { name } => {
            let _ = boxes::config_lenient(instance);
            boxes::stop(instance, &name).await?;
            Ok(0)
        }
        Command::Rm { name, force } => {
            boxes::rm(instance, &name, force).await?;
            Ok(0)
        }
        Command::Exec {
            name,
            no_start,
            interactive,
            tty,
            workdir,
            env,
            command,
        } => {
            let opts = ExecArgs {
                no_start,
                stdin: interactive,
                tty,
                workdir,
                env,
            };
            exec_cmd(instance, &name, opts, command).await
        }
        Command::Enter {
            name,
            no_start,
            workdir,
            env,
        } => {
            let opts = ExecArgs {
                no_start,
                stdin: true,
                tty: true,
                workdir,
                env,
            };
            exec_cmd(instance, &name, opts, vec!["bash".into()]).await
        }
        Command::Claude { name, args } => agent(instance, &name, "claude", args).await,
        Command::Opencode { name, args } => agent(instance, &name, "opencode", args).await,
        Command::Cp { src, dest } => {
            cp_cmd(instance, &src, &dest).await?;
            Ok(0)
        }
        Command::Ls { json } => {
            let boxes = boxes::ls(instance).await?;
            if json {
                print_json(&serde_json::json!({ "boxes": boxes }))?;
            } else if boxes.is_empty() {
                println!("no boxes");
            } else {
                let w = boxes.iter().map(|b| b.name.len()).max().unwrap_or(4).max(4);
                println!("{:w$}  {:8}  CONFIG", "NAME", "STATE");
                for b in boxes {
                    let changed = if b.config_changed { "changed" } else { "" };
                    println!("{:w$}  {:8}  {changed}", b.name, b.state);
                }
            }
            if !json {
                if let Some(config) = boxes::config_lenient(instance) {
                    if let Some(status) = builder::Builder::new(instance, &config).status().await? {
                        eprintln!("builder: {status}");
                    }
                }
            }
            Ok(0)
        }
        Command::Ports { name, json } => {
            let report = boxes::ports(instance, &name).await?;
            if json {
                print_json(&report)?;
            } else {
                for p in &report.ports {
                    let state = serde_json::to_value(p.state)?;
                    println!(
                        "{:>5}  {:9}  {}",
                        p.guest,
                        state.as_str().unwrap_or(""),
                        p.address.as_deref().unwrap_or("-")
                    );
                }
            }
            Ok(0)
        }
        Command::Install => {
            check_host()?;
            let config = boxes::config_strict(instance)?;
            let b = builder::Builder::new(instance, &config);
            let nix = b.ensure().await?;
            b.ensure_tools(&config, &nix).await?;
            eprintln!("bento: ready");
            Ok(0)
        }
        Command::Builder { action } => {
            let config = boxes::config_strict(instance)?;
            let b = builder::Builder::new(instance, &config);
            match action {
                None => {
                    need_tty("bento builder")?;
                    b.enter().await
                }
                Some(BuilderAction::Restart) => {
                    b.restart().await?;
                    Ok(0)
                }
                Some(BuilderAction::Rm) => {
                    b.remove().await?;
                    Ok(0)
                }
            }
        }
    }
}

async fn box_out(instance: &Instance, st: &state::BoxState, json: bool) -> Result<u8> {
    let config = config::load(instance).ok();
    let rec = boxes::record(instance, st, config.as_ref()).await?;
    if json {
        print_json(&rec)?;
    } else {
        eprintln!("bento: box '{}': {}", rec.name, rec.state);
    }
    Ok(0)
}

fn need_tty(what: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        return Err(fail::usage(format!("`{what}` needs a terminal")));
    }
    Ok(())
}

fn check_host() -> Result<()> {
    let problems: Vec<String> = microsandbox::setup::diagnose()
        .problems
        .iter()
        .map(|p| format!("{}\n  {}", p.headline, p.hints.join("\n  ")))
        .collect();
    if !problems.is_empty() {
        anyhow::bail!("this host can't run boxes:\n{}", problems.join("\n"));
    }
    Ok(())
}

struct ExecArgs {
    no_start: bool,
    stdin: bool,
    tty: bool,
    workdir: Option<String>,
    env: Vec<String>,
}

/// The token the agent shortcuts pass through from the caller's environment
/// (by name, as `-e`: the value never goes into an argument).
const AGENT_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// `exec`/`enter` `-e`s, each `K=V` or `K` from the caller's environment.
fn exec_env(
    env: &[String],
    caller: impl Fn(&str) -> Option<std::ffi::OsString> + Copy,
) -> Result<Vec<(String, String)>> {
    let mut out = vec![];
    for e in env {
        out.extend(names::parse_exec_env_arg(e, caller)?);
    }
    Ok(out)
}

/// The env of a caller's command: bento's own and the global env as of the
/// box's boot, its env_files read now, the box's `create -e`, then `-e`.
/// An env_files entry that `-e` overrides isn't read.
fn command_env(
    st: &state::BoxState,
    extra: &[(String, String)],
    tty: bool,
) -> Result<Vec<(String, String)>> {
    let boot = st
        .boot
        .as_ref()
        .ok_or_else(|| fail::fail(fail::OTHER, "the box has never booted"))?;
    let mut files = vec![];
    for (name, path) in &boot.env_files {
        if extra.iter().any(|(k, _)| k == name) {
            continue;
        }
        let mut v = std::fs::read_to_string(path).map_err(|e| {
            fail::fail(
                fail::OTHER,
                format!(
                    "env_files.{name}: {}: {e}; the command didn't run",
                    path.display()
                ),
            )
        })?;
        if v.ends_with('\n') {
            v.pop();
            if v.ends_with('\r') {
                v.pop();
            }
        }
        files.push((name.clone(), v));
    }
    let term = if tty {
        vec![(
            "TERM".to_string(),
            std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
        )]
    } else {
        vec![]
    };
    Ok(guest::merge_env(&[
        &term, &boot.env, &files, &st.env, extra,
    ]))
}

async fn exec_cmd(instance: &Instance, name: &str, a: ExecArgs, argv: Vec<String>) -> Result<u8> {
    names::check_box(name)?;
    let extra = exec_env(&a.env, |k| std::env::var_os(k))?;
    let cwd = a.workdir.unwrap_or_else(|| guest::WORKSPACE.to_string());
    if !cwd.starts_with('/') {
        return Err(fail::usage(format!("-w {cwd}: must be absolute")));
    }
    if a.tty {
        need_tty("bento exec -t")?;
    }
    if argv.is_empty() {
        return Err(fail::usage("no command"));
    }
    let (sb, st, _config) = boxes::ensure_running(instance, name, a.no_start).await?;
    let env = command_env(&st, &extra, a.tty)?;
    exec::run(
        &sb,
        exec::Command {
            argv,
            cwd,
            env,
            stdin: a.stdin,
            tty: a.tty,
        },
    )
    .await
}

async fn agent(instance: &Instance, name: &str, agent: &str, args: Vec<String>) -> Result<u8> {
    let (opts, argv) = agent_exec(agent, args, std::io::stdin().is_terminal());
    exec_cmd(instance, name, opts, argv).await
}

/// `bento claude|opencode`: `exec -i [-t] -e CLAUDE_CODE_OAUTH_TOKEN -- <agent> …`.
fn agent_exec(agent: &str, args: Vec<String>, tty: bool) -> (ExecArgs, Vec<String>) {
    let mut argv = vec![agent.to_string()];
    argv.extend(args);
    let opts = ExecArgs {
        no_start: false,
        stdin: true,
        tty,
        workdir: None,
        env: vec![AGENT_TOKEN_ENV.to_string()],
    };
    (opts, argv)
}

async fn cp_cmd(instance: &Instance, src: &str, dest: &str) -> Result<()> {
    use cp::Operand;
    match (cp::parse_operand(src), cp::parse_operand(dest)) {
        (Operand::Host(host), Operand::Box { name, path }) => {
            copy_in(instance, &host, src, &name, &path).await
        }
        (Operand::Box { name, path }, Operand::Host(host)) => {
            copy_out(instance, &name, &path, &host, dest).await
        }
        _ => Err(fail::usage(
            "cp: exactly one side must be a box path (<name>:<path>)",
        )),
    }
}

fn check_guest_path(p: &str) -> Result<()> {
    if !p.starts_with('/') {
        return Err(fail::usage(format!("cp: box paths are absolute: {p}")));
    }
    Ok(())
}

async fn guest_kind(
    sb: &microsandbox::sandbox::Sandbox,
    env: &[(String, String)],
    path: &str,
) -> Result<cp::Kind> {
    let out = exec::script(sb, guest::STAT, &[path], env)
        .await?
        .check("cp")?;
    cp::parse_kind(&String::from_utf8_lossy(&out.stdout))
}

async fn copy_in(
    instance: &Instance,
    host: &std::path::Path,
    src_arg: &str,
    name: &str,
    guest_dest: &str,
) -> Result<()> {
    names::check_box(name)?;
    check_guest_path(guest_dest)?;
    let kind = cp::host_kind(host);
    if matches!(kind, cp::Kind::Missing | cp::Kind::NoParent) {
        return Err(fail::fail(
            fail::OTHER,
            format!("{}: no such file or directory", host.display()),
        ));
    }
    let (sb, st, _) = boxes::ensure_running(instance, name, false).await?;
    let env = command_env(&st, &[], false)?;
    let dest_kind = guest_kind(&sb, &env, guest_dest).await?;
    let place = cp::place(src_arg, kind, guest_dest, dest_kind)?;
    let host = host.to_path_buf();
    let top = place.name.clone();
    exec::script_with_input(&sb, guest::EXTRACT, &[&place.dir], &env, move |w| {
        cp::write_archive(&host, top.as_deref(), w)
    })
    .await?
    .check("cp")
    .map_err(|e| fail::fail(fail::OTHER, format!("{e:#}")))?;
    Ok(())
}

async fn copy_out(
    instance: &Instance,
    name: &str,
    guest_src: &str,
    host: &std::path::Path,
    dest_arg: &str,
) -> Result<()> {
    names::check_box(name)?;
    check_guest_path(guest_src)?;
    let (sb, st, _) = boxes::ensure_running(instance, name, false).await?;
    let env = command_env(&st, &[], false)?;
    let src_kind = guest_kind(&sb, &env, guest_src).await?;
    let place = cp::place(guest_src, src_kind, dest_arg, cp::host_kind(host))?;
    let (adir, aname) = cp::archive_root(guest_src);
    let dest_dir = PathBuf::from(&place.dir);
    let top = aname.clone();
    let (code, err) =
        exec::script_with_output(&sb, guest::ARCHIVE, &[&adir, &aname], &env, move |r| {
            cp::unpack(r, &top, &dest_dir, place.name.as_deref())
        })
        .await?;
    if code != 0 {
        return Err(fail::fail(
            fail::OTHER,
            format!("cp: {}", String::from_utf8_lossy(&err).trim()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_the_contract() {
        let p = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("bento").chain(args.iter().copied()))
        };
        let c = p(&[
            "exec",
            "b",
            "--no-start",
            "-i",
            "-w",
            "/x",
            "-e",
            "A=1",
            "--",
            "sh",
            "-c",
            "echo -e",
        ])
        .unwrap();
        match c.command {
            Command::Exec {
                name,
                no_start,
                interactive,
                tty,
                workdir,
                env,
                command,
            } => {
                assert_eq!(name, "b");
                assert!(no_start && interactive && !tty);
                assert_eq!(workdir.as_deref(), Some("/x"));
                assert_eq!(env, ["A=1"]);
                assert_eq!(command, ["sh", "-c", "echo -e"]);
            }
            _ => panic!(),
        }
        // flags after the command belong to the command
        match p(&["exec", "b", "ls", "-i", "-w"]).unwrap().command {
            Command::Exec {
                interactive,
                command,
                ..
            } => {
                assert!(!interactive);
                assert_eq!(command, ["ls", "-i", "-w"]);
            }
            _ => panic!(),
        }
        assert!(p(&["exec", "b"]).is_err());
        assert!(p(&["create", "b", "-e", "A=1", "-e", "B=2", "--json"]).is_ok());
        assert!(p(&["rm", "b", "--force"]).is_ok());
        assert!(p(&["restart", "b", "--json"]).is_ok());
        assert!(p(&["cp", "a:/x", "./y"]).is_ok());
        match p(&["claude", "b", "--resume", "-p", "x"]).unwrap().command {
            Command::Claude { args, .. } => assert_eq!(args, ["--resume", "-p", "x"]),
            _ => panic!(),
        }
        let v = p(&["--version"]).unwrap_err();
        assert_eq!(v.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(v.to_string().contains("(contract 2)"));
    }

    #[test]
    fn command_env_order_and_env_files() {
        let d = std::env::temp_dir().join(format!("bento-env-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("tok"), "secret\n").unwrap();
        let st = state::BoxState {
            name: "b".into(),
            msb_name: "bento-b".into(),
            ip: std::net::Ipv4Addr::new(127, 77, 0, 1),
            env: vec![("A".into(), "box".into()), ("B".into(), "box".into())],
            phase: state::Phase::Ready,
            boot: Some(state::Boot {
                config_hash: "h".into(),
                env: vec![
                    ("PATH".into(), "/p".into()),
                    ("A".into(), "global".into()),
                    ("G".into(), "g".into()),
                ],
                env_files: vec![("TOK".into(), d.join("tok"))],
            }),
            ports: Default::default(),
        };
        let env = command_env(&st, &[("B".into(), "exec".into())], false).unwrap();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("A"), Some("box"));
        assert_eq!(get("B"), Some("exec"));
        assert_eq!(get("G"), Some("g"));
        assert_eq!(get("TOK"), Some("secret"));
        assert_eq!(get("PATH"), Some("/p"));
        assert_eq!(get("TERM"), None);
        // -e TOK (from the caller) wins over env_files.TOK
        let env = command_env(&st, &[("TOK".into(), "caller".into())], false).unwrap();
        let tok: Vec<_> = env.iter().filter(|(n, _)| n == "TOK").collect();
        assert_eq!(tok, [&("TOK".to_string(), "caller".to_string())]);
        std::fs::remove_file(d.join("tok")).unwrap();
        let err = command_env(&st, &[], false).unwrap_err();
        assert_eq!(fail::code_of(&err), fail::OTHER);
        // an overridden env_files file isn't read: its absence doesn't matter
        assert!(command_env(&st, &[("TOK".into(), "caller".into())], false).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn exec_env_from_the_caller() {
        use std::ffi::OsString;
        let caller = |k: &str| (k == "TOK").then(|| OsString::from("s3cret-value"));
        let env = exec_env(&["A=1".into(), "TOK".into(), "UNSET".into()], caller).unwrap();
        assert_eq!(
            env,
            [
                ("A".to_string(), "1".to_string()),
                ("TOK".to_string(), "s3cret-value".to_string())
            ]
        );
        let err = exec_env(&["TOK".into(), "HOME".into()], caller).unwrap_err();
        assert_eq!(fail::code_of(&err), fail::USAGE);
        assert!(!format!("{err:#}").contains("s3cret"));
        // `enter` takes -e too
        let c = Cli::try_parse_from(["bento", "enter", "b", "-e", "TOK", "-e", "A=1"]).unwrap();
        match c.command {
            Command::Enter { env, .. } => assert_eq!(env, ["TOK", "A=1"]),
            _ => panic!(),
        }
    }

    #[test]
    fn agent_shortcuts_pass_the_token_by_name() {
        let (opts, argv) = agent_exec("claude", vec!["-p".into(), "hi".into()], false);
        assert_eq!(argv, ["claude", "-p", "hi"]);
        assert_eq!(opts.env, [AGENT_TOKEN_ENV]);
        assert!(opts.stdin && !opts.tty);
        // the value reaches only the env, never argv
        let caller = |k: &str| (k == AGENT_TOKEN_ENV).then(|| "s3cret-value".into());
        let env = exec_env(&opts.env, caller).unwrap();
        assert_eq!(
            env,
            [(AGENT_TOKEN_ENV.to_string(), "s3cret-value".to_string())]
        );
        assert!(!argv.iter().any(|a| a.contains("s3cret")));
        // unset in the caller: not set, the box's own (the default claude account's) applies
        assert!(exec_env(&opts.env, |_| None).unwrap().is_empty());
    }
}
