//! `sybild`: the assistant's daemon and gateway in one process.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use xshell_gateway::config::Config;
use xshell_gateway::directory::Directory;
use xshell_gateway::gateway::Gateway;
use xshell_gateway::schedule::ScheduleStore;
use xshell_gateway::telegram::Telegram;

#[derive(Debug, Parser)]
#[command(version, about = "A multi-user chat assistant built on xshell")]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the session daemon and the chat gateway until stopped.
    Run {
        #[arg(long)]
        config: PathBuf,
    },
    /// Check a configuration file and report what it sets up.
    Check {
        #[arg(long)]
        config: PathBuf,
    },
    /// Run an xshelld command against this build, such as `probe` or
    /// `serve-stdio`: sybild daemon -- --state-directory DIR probe
    Daemon {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
}

fn main() -> Result<()> {
    match Arguments::parse().command {
        Command::Run { config } => run(Config::load(&config)?),
        Command::Check { config } => check(&Config::load(&config)?),
        Command::Daemon { arguments } => {
            let mut command_line = vec![OsString::from("sybild")];
            command_line.extend(arguments);
            xshell_session::daemon::Daemon::new().run_with_args(command_line)
        }
    }
}

fn run(config: Config) -> Result<()> {
    config.check_shell_ceiling()?;
    let config = Arc::new(config);
    let directory = Arc::new(Directory::default());
    let store = Arc::new(ScheduleStore::new(&config.workspace));
    let daemon = xshell_gateway::daemon(&config, Arc::clone(&directory), Arc::clone(&store))?;
    // Read the token before anything starts, so a missing one stops the
    // process instead of leaving a daemon nobody can talk to.
    let telegram = match &config.telegram {
        Some(telegram) => Some(Telegram::from_env(&telegram.token_env)?),
        None => {
            eprintln!("sybild: no [telegram] section; running the session daemon only");
            None
        }
    };
    if let Some(telegram) = telegram {
        let gateway = Gateway::new(Arc::clone(&config), directory, store, Arc::new(telegram));
        // The gateway is a thread of the daemon's own process, which is
        // what lets it connect: the daemon refuses its child processes.
        thread::Builder::new()
            .name("gateway".into())
            .spawn(move || {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("cannot start the gateway's runtime")
                    .block_on(gateway.run());
            })
            .context("cannot start the gateway thread")?;
    }
    // The daemon owns the process from here: it handles SIGTERM and SIGINT
    // and returns once its sessions are drained. The gateway thread ends
    // with the process.
    daemon.run_with_args(xshell_gateway::daemon_arguments(&config))
}

fn check(config: &Config) -> Result<()> {
    println!("workspace: {}", config.workspace.display());
    for name in ["SOUL.md", "IDENTITY.md", "AGENTS.md"] {
        let present = config.workspace.join(name).is_file();
        println!("  {name}: {}", if present { "found" } else { "missing" });
    }
    println!("state directory: {}", config.state_directory.display());
    println!(
        "model: {} {} at {}",
        config.model.provider, config.model.model, config.model.base_url
    );
    match &config.telegram {
        Some(telegram) => {
            let set = std::env::var_os(&telegram.token_env).is_some_and(|token| !token.is_empty());
            println!(
                "telegram: token from {} ({})",
                telegram.token_env,
                if set { "set" } else { "NOT SET" }
            );
        }
        None => println!("telegram: not configured"),
    }
    match &config.heartbeat {
        Some(heartbeat) => println!(
            "heartbeat: every {} minutes{}",
            heartbeat.every_minutes,
            heartbeat
                .active_hours
                .as_deref()
                .map(|hours| format!(", {hours} local time"))
                .unwrap_or_default()
        ),
        None => println!("heartbeat: off"),
    }
    match &config.web_search {
        Some(search) => println!("web search: {}", search.backend),
        None => println!("web search: off"),
    }
    println!("users: {}", config.users.len());
    for user in &config.users {
        let account = match user.telegram_id {
            Some(id) => format!("Telegram {id}"),
            None => "no chat account".to_owned(),
        };
        println!(
            "  {} ({account}): {}{}",
            user.name,
            config.tool_groups(&user.name).join(", "),
            match user.shell {
                Some(_) => ", UNRESTRICTED SHELL",
                None => "",
            }
        );
    }
    if let Err(problem) = config.check_shell_ceiling() {
        println!("PROBLEM: {problem:#}");
    }
    Ok(())
}
