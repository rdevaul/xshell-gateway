//! A multi-user chat assistant built on the xshell session daemon.
//!
//! One process holds everything:
//!
//! - the **session daemon** ([`xshell_session::daemon::Daemon`]), which runs
//!   agent turns, enforces each session's tools, and keeps conversations;
//! - the **memory tools** ([`memory::MemoryProvider`]) and the **per-turn
//!   context** ([`context::WorkspaceContext`]), compiled into that daemon;
//! - the **gateway** ([`gateway::Gateway`]), which gives each user one
//!   session and carries messages between a chat service and it.
//!
//! See `docs/architecture.md`.

pub mod config;
pub mod context;
pub mod cron;
pub mod directory;
pub mod gateway;
pub mod heartbeat;
pub mod memory;
pub mod schedule;
pub mod scheduler;
pub mod telegram;
pub mod web;
pub mod workspace;

use anyhow::Result;
use std::ffi::OsString;
use std::sync::Arc;
use xshell_execution::ToolRegistry;
use xshell_session::daemon::Daemon;

/// The session daemon with the assistant's tools and context compiled in.
/// `directory` and `store` are shared with the gateway, which records each
/// user's session in the one and runs due tasks from the other.
///
/// Every tool group is compiled in; which of them a user's agent can use is
/// decided by that user's session (see [`config::Config::session_tools`]).
pub fn daemon(
    config: &Arc<config::Config>,
    directory: Arc<directory::Directory>,
    store: Arc<schedule::ScheduleStore>,
) -> Result<Daemon> {
    let workspace = workspace::Workspace::new(config.workspace.clone());
    let mut tools = ToolRegistry::builder()
        .provider(Arc::new(memory::MemoryProvider::new(
            workspace.clone(),
            Arc::clone(&directory),
        )))
        .provider(Arc::new(schedule::ScheduleProvider::new(
            store,
            Arc::clone(&directory),
        )));
    if let Some(search) = &config.web_search {
        tools = tools.provider(Arc::new(web::WebProvider::new(web::backend(search)?)));
    }
    Ok(Daemon::new()
        .with_tools(tools.build()?)
        .with_context(Arc::new(context::WorkspaceContext::new(
            workspace,
            directory,
            Arc::clone(config),
        ))))
}

/// The embedded daemon's command line: its state and socket come from the
/// gateway's configuration, so both sides agree on where the socket is.
pub fn daemon_arguments(config: &config::Config) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = vec![
        "sybild".into(),
        "--state-directory".into(),
        config.state_directory.clone().into(),
        "--socket".into(),
        config.socket().into(),
    ];
    match &config.xshell_config {
        Some(path) => arguments.extend(["--config".into(), path.clone().into()]),
        None => arguments.push("--no-user-config".into()),
    }
    arguments
}
