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
use crate::schedule::{ScheduleStore, Task};
use crate::workspace::Workspace;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
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

/// Work for one user's agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// A message the user sent.
    Chat(Inbound),
    /// A periodic check-in (see [`crate::heartbeat`]).
    Heartbeat,
    /// A scheduled task that has come due (see [`crate::schedule`]).
    Scheduled(Task),
}

pub struct Gateway<T: Transport> {
    dispatcher: Arc<Dispatcher<T>>,
    store: Arc<ScheduleStore>,
}

impl<T: Transport> Gateway<T> {
    /// `store` must be the one the daemon's schedule tools write to.
    pub fn new(
        config: Arc<Config>,
        directory: Arc<Directory>,
        store: Arc<ScheduleStore>,
        transport: Arc<T>,
    ) -> Self {
        Self {
            dispatcher: Arc::new(Dispatcher {
                config,
                directory,
                transport,
                runtime: OnceLock::new(),
                workers: Mutex::new(HashMap::new()),
            }),
            store,
        }
    }

    /// Where jobs are handed to users' workers; tests use it to run a
    /// heartbeat or a task without waiting for the clock.
    pub fn dispatcher(&self) -> Arc<Dispatcher<T>> {
        Arc::clone(&self.dispatcher)
    }

    /// Run heartbeats and scheduled tasks on the clock, and receive and
    /// route messages, until the process ends. A failed receive is logged
    /// and retried after a pause.
    pub async fn run(self) {
        // Workers answer on this runtime, whichever thread queues their job.
        let _ = self
            .dispatcher
            .runtime
            .set(tokio::runtime::Handle::current());
        tokio::spawn(crate::scheduler::run(
            Arc::clone(&self.dispatcher),
            Arc::clone(&self.store),
        ));
        let transport = Arc::clone(&self.dispatcher.transport);
        loop {
            match transport.receive().await {
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

    async fn route(&self, message: Inbound) {
        let config = &self.dispatcher.config;
        let Some(user) = config.user_for_telegram(message.sender) else {
            eprintln!(
                "gateway: refused a message from unknown account {}",
                message.sender
            );
            let sent = self
                .dispatcher
                .transport
                .send(message.chat, REFUSED_REPLY)
                .await;
            if let Err(error) = sent {
                eprintln!(
                    "gateway: cannot answer account {}: {error:#}",
                    message.sender
                );
            }
            return;
        };
        self.dispatcher.enqueue(&user.name, Job::Chat(message));
    }
}

/// Hands jobs to each user's worker, starting workers as needed.
pub struct Dispatcher<T: Transport> {
    config: Arc<Config>,
    directory: Arc<Directory>,
    transport: Arc<T>,
    /// The gateway's async runtime, set when it starts running.
    runtime: OnceLock<tokio::runtime::Handle>,
    workers: Mutex<HashMap<String, WorkerHandle>>,
}

struct WorkerHandle {
    jobs: mpsc::Sender<Job>,
    /// Jobs queued or running, so the scheduler can tell a busy user.
    pending: Arc<AtomicUsize>,
}

impl<T: Transport> Dispatcher<T> {
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Queue `job` for `user`. Jobs for one user run in the order queued.
    pub fn enqueue(&self, user: &str, job: Job) {
        let Some(runtime) = self.runtime.get().cloned() else {
            eprintln!("gateway: dropped a job for {user}: the gateway is not running yet");
            return;
        };
        let mut workers = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = workers.entry(user.to_owned()).or_insert_with(|| {
            let (jobs, queue) = mpsc::channel();
            let pending = Arc::new(AtomicUsize::new(0));
            let worker = Worker {
                user: user.to_owned(),
                config: Arc::clone(&self.config),
                directory: Arc::clone(&self.directory),
                transport: Arc::clone(&self.transport),
                runtime: runtime.clone(),
                pending: Arc::clone(&pending),
                session: None,
            };
            thread::Builder::new()
                .name(format!("chat-{user}"))
                .spawn(move || worker.run(queue))
                .expect("cannot start a user's worker thread");
            WorkerHandle { jobs, pending }
        });
        handle.pending.fetch_add(1, Ordering::AcqRel);
        if handle.jobs.send(job).is_err() {
            eprintln!("gateway: {user}'s worker stopped; restarting it on the next job");
            workers.remove(user);
        }
    }

    /// Whether `user` has nothing queued or running.
    pub fn is_idle(&self, user: &str) -> bool {
        self.workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(user)
            .is_none_or(|handle| handle.pending.load(Ordering::Acquire) == 0)
    }
}

/// One user's connection to the daemon and their session on it.
struct Worker<T: Transport> {
    user: String,
    config: Arc<Config>,
    directory: Arc<Directory>,
    transport: Arc<T>,
    runtime: tokio::runtime::Handle,
    pending: Arc<AtomicUsize>,
    session: Option<Session>,
}

struct Session {
    client: SessionClient,
    id: String,
    /// The last event sequence this worker has read.
    after: u64,
}

/// How a turn ended, when the daemon could be reached.
enum Outcome {
    Answer(String),
    /// The turn failed or was cancelled; the reason is in the log.
    Failed,
}

impl<T: Transport> Worker<T> {
    fn run(mut self, jobs: mpsc::Receiver<Job>) {
        for job in jobs {
            if let Some((chat, text)) = self.handle(job) {
                let sent = self.runtime.block_on(self.transport.send(chat, &text));
                if let Err(error) = sent {
                    eprintln!("gateway: cannot send to {}: {error:#}", self.user);
                }
            }
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Run `job` and return what to send, and where, if anything.
    fn handle(&mut self, job: Job) -> Option<(i64, String)> {
        // Heartbeats and tasks have no incoming chat to answer; they go to
        // the user's own private chat, whose id is their account's.
        let own_chat = self
            .config
            .user(&self.user)
            .and_then(|user| user.telegram_id);
        match job {
            Job::Chat(message) => {
                let reply = match self.turn(&message.text) {
                    Outcome::Answer(answer) => answer,
                    Outcome::Failed => FAILED_REPLY.to_owned(),
                };
                Some((message.chat, reply))
            }
            Job::Heartbeat => {
                let chat = own_chat?;
                let workspace = Workspace::new(self.config.workspace.clone());
                let prompt = match crate::heartbeat::prompt(&workspace, &self.user) {
                    Ok(prompt) => prompt?,
                    Err(error) => {
                        eprintln!("gateway: {}'s heartbeat: {error:#}", self.user);
                        return None;
                    }
                };
                // A failed heartbeat is not worth a message; it is logged.
                match self.turn(&prompt) {
                    Outcome::Answer(answer) => {
                        crate::heartbeat::visible(&answer).map(|text| (chat, text))
                    }
                    Outcome::Failed => None,
                }
            }
            Job::Scheduled(task) => {
                let chat = own_chat?;
                let prompt = format!(
                    "[Scheduled task {}, which you set up for {} on {}. This is not a new \
message from them; your answer will be sent to them.]\n\n{}",
                    task.id, self.user, task.created, task.prompt
                );
                let reply = match self.turn(&prompt) {
                    Outcome::Answer(answer) => answer,
                    Outcome::Failed => format!(
                        "Sorry, your scheduled task {} couldn't run: {}",
                        task.id, task.prompt
                    ),
                };
                Some((chat, reply))
            }
        }
    }

    /// Run one turn for `text`. A failure to reach the daemon is logged and
    /// reported as a failed turn, and the connection is reopened next time.
    fn turn(&mut self, text: &str) -> Outcome {
        match self.try_turn(text) {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("gateway: {}'s turn failed: {error:#}", self.user);
                self.session = None;
                Outcome::Failed
            }
        }
    }

    fn try_turn(&mut self, text: &str) -> Result<Outcome> {
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
                        return Ok(match answer.filter(|answer| !answer.trim().is_empty()) {
                            Some(answer) => Outcome::Answer(answer),
                            None => {
                                eprintln!(
                                    "gateway: {}'s turn {turn_id} ended with no answer",
                                    self.user
                                );
                                Outcome::Failed
                            }
                        });
                    }
                    SessionEventKind::TurnFailed { message, .. } => {
                        eprintln!("gateway: {}'s turn {turn_id} failed: {message}", self.user);
                        return Ok(Outcome::Failed);
                    }
                    SessionEventKind::TurnCancelled => {
                        eprintln!("gateway: {}'s turn {turn_id} was cancelled", self.user);
                        return Ok(Outcome::Failed);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Connect to the daemon and attach this user's session, creating it the
    /// first time and bringing an older one up to date.
    fn open_session(&self) -> Result<Session> {
        let mut client = connect(&self.config.socket())?;
        let name = session_name(&self.user);
        let model = self.config.model.binding();
        let tools = SessionTools::Only(crate::session_tools());
        let creation = |history: Vec<ChatMessage>| -> Result<SessionCreation> {
            Ok(SessionCreation {
                name: name.clone(),
                model: model.clone(),
                cwd: Workspace::new(self.config.workspace.clone()).ensure_user(&self.user)?,
                // Kept across restarts, so a conversation continues.
                persistence: PersistenceMode::Durable,
                // Never offered to other hosts.
                visibility: Visibility::HostOnly,
                // The system prompt comes from the context provider on
                // every turn; the session itself carries none.
                history,
                tools: tools.clone(),
            })
        };
        let existing = client
            .list()?
            .into_iter()
            .any(|session| session.name == name);
        let snapshot = if !existing {
            client.create(creation(Vec::new())?)?
        } else {
            let snapshot = client.attach(name.clone())?;
            if snapshot.descriptor.tools != tools {
                // A session's tools are fixed for its life, so a session
                // from a build with other tools is replaced by one with
                // today's, keeping its conversation.
                eprintln!(
                    "gateway: replacing {}'s session to give it the current tools",
                    self.user
                );
                client.close(None)?;
                client.create(creation(snapshot.history)?)?
            } else if snapshot.descriptor.model != model {
                // The configured model changed; the conversation carries on
                // with the new one.
                client.update(
                    snapshot.descriptor.id.clone(),
                    model,
                    snapshot.descriptor.cwd.clone(),
                    snapshot.history.clone(),
                )?;
                snapshot
            } else {
                snapshot
            }
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
