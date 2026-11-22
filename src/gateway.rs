//! Routes chat messages to each user's session on the daemon and sends the
//! agent's answer back.
//!
//! Every user has one durable session, limited to the memory tools. A user's
//! messages are handled one at a time by that user's own worker thread, with
//! its own connection to the daemon: a daemon connection controls one
//! session, and the client is blocking. Different users' turns run at the
//! same time, up to the daemon's own limit.

use crate::config::Config;
use crate::directory::{Directory, session_name};
use crate::memory::TOOL_NAMES;
use crate::workspace::Workspace;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use xshell_core::ChatMessage;
use xshell_execution::{
    ApprovalDecision, ApprovalPolicy, ApprovalSource, Approver, ExecutionEvent,
};
use xshell_session::{
    PersistenceMode, SessionClient, SessionCreation, SessionEventKind, SessionTools, TurnInput,
    Visibility,
};

/// How long a worker waits for the embedded daemon to accept connections.
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one events request waits for something to happen.
const EVENT_WAIT_MS: u64 = 1000;
/// What a user is told when their turn fails. Details go to the log: a
/// failure message can quote paths, endpoints, or provider errors.
const FAILED_REPLY: &str = "Sorry, something went wrong and I couldn't answer that.";
const REFUSED_REPLY: &str = "Sorry, I'm not set up to talk with this account.";

/// A message a person sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    /// The sender's account on the transport.
    pub sender: i64,
    /// Where the answer goes.
    pub chat: i64,
    pub text: String,
}

/// A chat service the gateway receives messages from and answers on.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Wait for the next messages. An empty list means none arrived yet.
    async fn receive(&self) -> Result<Vec<Inbound>>;
    async fn send(&self, chat: i64, text: &str) -> Result<()>;
}

pub struct Gateway<T: Transport> {
    config: Arc<Config>,
    directory: Arc<Directory>,
    transport: Arc<T>,
    workers: HashMap<String, mpsc::Sender<Inbound>>,
}

impl<T: Transport> Gateway<T> {
    pub fn new(config: Arc<Config>, directory: Arc<Directory>, transport: Arc<T>) -> Self {
        Self {
            config,
            directory,
            transport,
            workers: HashMap::new(),
        }
    }

    /// Receive and route messages until the transport fails for good. A
    /// failed receive is logged and retried after a pause.
    pub async fn run(mut self) {
        loop {
            match self.transport.receive().await {
                Ok(messages) => {
                    for message in messages {
                        self.route(message).await;
                    }
                }
                Err(error) => {
                    eprintln!("gateway: cannot receive messages: {error:#}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn route(&mut self, message: Inbound) {
        let Some(user) = self.config.user_for_telegram(message.sender) else {
            eprintln!(
                "gateway: refused a message from unknown account {}",
                message.sender
            );
            if let Err(error) = self.transport.send(message.chat, REFUSED_REPLY).await {
                eprintln!(
                    "gateway: cannot answer account {}: {error:#}",
                    message.sender
                );
            }
            return;
        };
        let name = user.name.clone();
        let worker = self.workers.entry(name.clone()).or_insert_with(|| {
            let (sender, receiver) = mpsc::channel();
            let worker = Worker {
                user: name.clone(),
                config: Arc::clone(&self.config),
                directory: Arc::clone(&self.directory),
                transport: Arc::clone(&self.transport),
                runtime: tokio::runtime::Handle::current(),
                session: None,
            };
            thread::Builder::new()
                .name(format!("chat-{name}"))
                .spawn(move || worker.run(receiver))
                .expect("cannot start a user's worker thread");
            sender
        });
        if worker.send(message).is_err() {
            eprintln!("gateway: {name}'s worker stopped; restarting it on the next message");
            self.workers.remove(&name);
        }
    }
}

/// One user's connection to the daemon and their session on it.
struct Worker<T: Transport> {
    user: String,
    config: Arc<Config>,
    directory: Arc<Directory>,
    transport: Arc<T>,
    runtime: tokio::runtime::Handle,
    session: Option<Session>,
}

struct Session {
    client: SessionClient,
    id: String,
    /// The last event sequence this worker has read.
    after: u64,
}

impl<T: Transport> Worker<T> {
    fn run(mut self, messages: mpsc::Receiver<Inbound>) {
        for message in messages {
            let reply = match self.answer(&message.text) {
                Ok(reply) => reply,
                Err(error) => {
                    eprintln!("gateway: {}'s turn failed: {error:#}", self.user);
                    // The connection or session may be gone; start afresh
                    // on the next message.
                    self.session = None;
                    FAILED_REPLY.to_owned()
                }
            };
            let sent = self
                .runtime
                .block_on(self.transport.send(message.chat, &reply));
            if let Err(error) = sent {
                eprintln!("gateway: cannot send {}'s answer: {error:#}", self.user);
            }
        }
    }

    /// Run one turn for `text` and return what to tell the user.
    fn answer(&mut self, text: &str) -> Result<String> {
        if self.session.is_none() {
            self.session = Some(self.open_session()?);
        }
        let session = self.session.as_mut().expect("the session was just opened");
        let turn_id = session.client.submit(
            session.id.clone(),
            TurnInput::Agent {
                message: text.to_owned(),
                images: Vec::new(),
            },
            // Chat sessions hold only tools that need no approval. `ask`
            // is still the right mode: anything that would prompt is denied
            // below, since nobody is at a terminal to answer.
            ApprovalPolicy::Ask,
        )?;
        let mut answer = None;
        loop {
            let batch = session
                .client
                .events(session.id.clone(), session.after, EVENT_WAIT_MS)?;
            for record in batch.events {
                session.after = record.sequence;
                if record.turn_id != turn_id {
                    continue;
                }
                match record.event {
                    SessionEventKind::Execution { event } => match event {
                        ExecutionEvent::AgentResponse {
                            content,
                            tool_call_count: 0,
                            partial: false,
                            ..
                        } => answer = Some(content),
                        ExecutionEvent::ApprovalRequested { call, .. } => {
                            session.client.approve(
                                session.id.clone(),
                                turn_id.clone(),
                                call.id,
                                ApprovalDecision::Deny,
                                ApprovalSource::new(Approver::NoApprover),
                            )?;
                        }
                        _ => {}
                    },
                    SessionEventKind::TurnCompleted => {
                        return Ok(answer
                            .filter(|answer| !answer.trim().is_empty())
                            .unwrap_or_else(|| FAILED_REPLY.to_owned()));
                    }
                    SessionEventKind::TurnFailed { message, .. } => {
                        eprintln!("gateway: {}'s turn {turn_id} failed: {message}", self.user);
                        return Ok(FAILED_REPLY.to_owned());
                    }
                    SessionEventKind::TurnCancelled => {
                        eprintln!("gateway: {}'s turn {turn_id} was cancelled", self.user);
                        return Ok(FAILED_REPLY.to_owned());
                    }
                    _ => {}
                }
            }
        }
    }

    /// Connect to the daemon and attach this user's session, creating it the
    /// first time.
    fn open_session(&self) -> Result<Session> {
        let mut client = connect(&self.config.socket())?;
        let name = session_name(&self.user);
        let existing = client
            .list()?
            .into_iter()
            .any(|session| session.name == name);
        let snapshot = if existing {
            client.attach(name)?
        } else {
            let cwd = Workspace::new(self.config.workspace.clone()).ensure_user(&self.user)?;
            client.create(SessionCreation {
                name,
                model: self.config.model.binding(),
                cwd,
                // Kept across restarts, so a conversation continues.
                persistence: PersistenceMode::Durable,
                // Never offered to other hosts.
                visibility: Visibility::HostOnly,
                // The system prompt comes from the context provider on
                // every turn; the session itself carries none.
                history: Vec::<ChatMessage>::new(),
                tools: SessionTools::Only(TOOL_NAMES.map(String::from).to_vec()),
            })?
        };
        let id = snapshot.descriptor.id;
        // From here on, the daemon's calls for this session are this user's.
        self.directory.register(&id, &self.user);
        // Events of earlier turns are read once and skipped by turn id.
        Ok(Session {
            client,
            id,
            after: 0,
        })
    }
}

/// Connect to the embedded daemon, waiting while it starts.
fn connect(socket: &std::path::Path) -> Result<SessionClient> {
    let started = Instant::now();
    loop {
        match SessionClient::connect(socket, env!("CARGO_PKG_VERSION")) {
            Ok(client) => return Ok(client),
            Err(error) if started.elapsed() >= DAEMON_READY_TIMEOUT => {
                return Err(error).with_context(|| {
                    format!(
                        "the session daemon at {} is not answering",
                        socket.display()
                    )
                });
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Split `text` into pieces of at most `limit` characters, at line breaks
/// where it can, for transports that cap a message's length.
pub fn chunks(text: &str, limit: usize) -> Result<Vec<&str>> {
    if limit == 0 {
        bail!("a message limit of zero cannot hold any text");
    }
    let mut pieces = Vec::new();
    let mut rest = text;
    while rest.chars().count() > limit {
        let hard_end = rest
            .char_indices()
            .nth(limit)
            .map_or(rest.len(), |(index, _)| index);
        // Prefer the last line break in the piece, unless it would leave
        // a piece under half the limit.
        let end = rest[..hard_end]
            .rfind('\n')
            .filter(|&index| rest[..index].chars().count() >= limit / 2)
            .map_or(hard_end, |index| index + 1);
        pieces.push(rest[..end].trim_end_matches('\n'));
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        pieces.push(rest);
    }
    Ok(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_piece_and_long_text_breaks_at_lines() {
        assert_eq!(chunks("hello", 10).unwrap(), ["hello"]);
        assert!(chunks("", 10).unwrap().is_empty());
        let text = "first line\nsecond line\nthird line";
        assert_eq!(
            chunks(text, 24).unwrap(),
            ["first line\nsecond line", "third line"]
        );
        // No line break to use: cut at the limit.
        assert_eq!(chunks("abcdefghij", 4).unwrap(), ["abcd", "efgh", "ij"]);
        assert!(chunks("x", 0).is_err());
    }

    #[test]
    fn pieces_never_split_a_character_and_lose_no_text() {
        let text = "é".repeat(25);
        let pieces = chunks(&text, 10).unwrap();
        assert_eq!(pieces.len(), 3);
        assert!(pieces.iter().all(|piece| piece.chars().count() <= 10));
        assert_eq!(pieces.concat(), text);
    }
}
