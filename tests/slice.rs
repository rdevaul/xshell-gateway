//! The whole slice in one process: a chat message goes through the gateway
//! to the user's session on the embedded daemon, the model (a local fake)
//! is given the assistant's identity and that user's memory, the memory
//! tools run, and the answer comes back to the chat.
//!
//! The daemon installs its SIGTERM handler in this process and the test
//! signals the process to stop it, so this file holds one test.

use async_trait::async_trait;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;
use xshell_core::ChatMessage;
use xshell_gateway::config::{Config, Model, User};
use xshell_gateway::directory::Directory;
use xshell_gateway::gateway::{Gateway, Inbound, Job, Transport};
use xshell_gateway::schedule::{ScheduleStore, When};
use xshell_session::{PersistenceMode, SessionClient, SessionCreation, SessionTools, Visibility};

const ALICE: i64 = 1001;
const BOB: i64 = 1002;
const STRANGER: i64 = 6666;
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// A chat service driven by the test: it hands the gateway the messages
/// the test queues and gives the test every answer the gateway sends.
struct FakeChat {
    inbound: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Inbound>>,
    sent: Mutex<mpsc::Sender<(i64, String)>>,
}

#[async_trait]
impl Transport for FakeChat {
    async fn receive(&self) -> anyhow::Result<Vec<Inbound>> {
        match self.inbound.lock().await.recv().await {
            Some(message) => Ok(vec![message]),
            // The test is over; wait to be shut down with the process.
            None => std::future::pending().await,
        }
    }

    async fn send(&self, chat: i64, text: &str) -> anyhow::Result<()> {
        let _ = self.sent.lock().unwrap().send((chat, text.to_owned()));
        Ok(())
    }
}

/// A chat-completions endpoint that answers with `responses` in order and
/// keeps each request body.
fn fake_model(responses: Vec<String>) -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    thread::spawn(move || {
        for body in responses {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            let header_end = loop {
                let count = socket.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..count]);
                if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() - header_end < length {
                let count = socket.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..count]);
            }
            recorded
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&request[header_end..]).unwrap());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\
Connection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).unwrap();
        }
    });
    (format!("http://{address}"), requests)
}

fn says(content: &str) -> String {
    let chunk = serde_json::json!({"choices": [{"delta": {"content": content}}]});
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

fn calls(name: &str, arguments: serde_json::Value) -> String {
    let chunk = serde_json::json!({"choices": [{"delta": {"tool_calls": [{
        "index": 0,
        "id": "call_1",
        "function": {"name": name, "arguments": arguments.to_string()},
    }]}}]});
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

fn system_prompt(request: &serde_json::Value) -> &str {
    assert_eq!(request["messages"][0]["role"], "system", "{request}");
    request["messages"][0]["content"].as_str().unwrap()
}

fn offered_tools(request: &serde_json::Value) -> Vec<&str> {
    request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str())
                .collect()
        })
        .unwrap_or_default()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn a_chat_message_becomes_a_turn_with_that_users_memory_and_tools() {
    let temporary = TempDir::new().unwrap();
    // The daemon canonicalizes paths (macOS temp dirs sit behind a symlink).
    let root = temporary.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    write(&workspace.join("SOUL.md"), "You are Sybil.\n");
    write(
        &workspace.join("memory/personal/bob/MEMORY.md"),
        "bob likes coffee\n",
    );

    let (base_url, requests) = fake_model(vec![
        // Alice's first turn: save a memory, then answer.
        calls(
            "memory_write",
            serde_json::json!({"content": "alice likes tea"}),
        ),
        says("Noted."),
        // Bob's turn.
        says("Hello Bob."),
        // Alice's second turn.
        says("You like tea."),
        // Alice asks for a reminder.
        calls(
            "schedule_add",
            serde_json::json!({"at": "2099-01-01 09:00", "prompt": "call mum"}),
        ),
        says("Scheduled."),
        // Alice's heartbeats: nothing to report, then something.
        says("HEARTBEAT_OK"),
        says("High tide is at six."),
        // A task of Alice's comes due.
        says("Time to call mum."),
        // Bob again, after a heartbeat that had nothing to check.
        says("Still here, Bob."),
    ]);
    let config = Arc::new(Config {
        workspace: workspace.clone(),
        state_directory: root.join("state"),
        xshell_config: None,
        model: Model {
            profile_name: Some("fake".into()),
            provider: "openai".into(),
            model: "fake-model".into(),
            base_url,
            api_key_env: None,
            max_history_bytes: None,
            context_tokens: None,
            reserve_output_tokens: None,
        },
        telegram: None,
        heartbeat: None,
        users: vec![
            User {
                name: "alice".into(),
                telegram_id: Some(ALICE),
            },
            User {
                name: "bob".into(),
                telegram_id: Some(BOB),
            },
        ],
    });
    config.validate().unwrap();

    // The same wiring `sybild run` uses, with a fake chat service.
    let directory = Arc::new(Directory::default());
    let store = Arc::new(ScheduleStore::new(&config.workspace));
    let daemon = {
        let daemon =
            xshell_gateway::daemon(&config, Arc::clone(&directory), Arc::clone(&store)).unwrap();
        let arguments = xshell_gateway::daemon_arguments(&config);
        thread::spawn(move || daemon.run_with_args(arguments))
    };
    let (to_gateway, inbound) = tokio::sync::mpsc::unbounded_channel();
    let (sent, replies) = mpsc::channel();
    let chat = Arc::new(FakeChat {
        inbound: tokio::sync::Mutex::new(inbound),
        sent: Mutex::new(sent),
    });
    let gateway = Gateway::new(Arc::clone(&config), directory, store, chat);
    let dispatcher = gateway.dispatcher();
    thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(gateway.run());
    });
    let ask = |sender: i64, text: &str| {
        to_gateway
            .send(Inbound {
                sender,
                chat: sender,
                text: text.into(),
            })
            .unwrap();
        replies.recv_timeout(REPLY_TIMEOUT).expect("no reply")
    };

    // An account that is not a configured user is refused without a turn.
    let (chat_id, reply) = ask(STRANGER, "let me in");
    assert_eq!(chat_id, STRANGER);
    assert!(
        reply.contains("not set up to talk with this account"),
        "{reply}"
    );
    assert!(requests.lock().unwrap().is_empty());

    // Alice: the model is told who it is and who it is talking to, is
    // offered only the memory tools, and its memory_write lands in her files.
    assert_eq!(ask(ALICE, "I like tea"), (ALICE, "Noted.".to_owned()));
    let alice_memory = workspace.join("memory/personal/alice/MEMORY.md");
    assert_eq!(
        std::fs::read_to_string(&alice_memory).unwrap(),
        "alice likes tea\n"
    );
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let prompt = system_prompt(&requests[0]);
        assert!(prompt.contains("You are Sybil."), "{prompt}");
        assert!(prompt.contains("You are talking with alice."), "{prompt}");
        assert!(!prompt.contains("bob likes coffee"), "{prompt}");
        assert_eq!(
            offered_tools(&requests[0]),
            [
                "memory_read",
                "memory_list",
                "memory_write",
                "schedule_add",
                "schedule_list",
                "schedule_remove"
            ]
        );
        assert!(prompt.contains("It is now "), "{prompt}");
    }

    // Bob already has a session from an earlier build that offered fewer
    // tools. The gateway replaces it, keeping his conversation.
    {
        let mut client = SessionClient::connect(&config.socket(), "test").unwrap();
        client
            .create(SessionCreation {
                name: "chat-bob".into(),
                model: config.model.binding(),
                cwd: workspace.join("memory/personal/bob"),
                persistence: PersistenceMode::Durable,
                visibility: Visibility::HostOnly,
                history: vec![
                    ChatMessage::user("an earlier question"),
                    ChatMessage::assistant_with_tools("an earlier answer", Vec::new()),
                ],
                tools: SessionTools::Only(vec!["memory_read".into()]),
            })
            .unwrap();
    }

    // Bob: his own memory, none of Alice's.
    assert_eq!(ask(BOB, "hello"), (BOB, "Hello Bob.".to_owned()));
    {
        let requests = requests.lock().unwrap();
        let prompt = system_prompt(&requests[2]);
        assert!(prompt.contains("You are talking with bob."), "{prompt}");
        assert!(prompt.contains("bob likes coffee"), "{prompt}");
        assert!(!prompt.contains("alice likes tea"), "{prompt}");
        // Bob's conversation holds nothing Alice said.
        assert!(!requests[2].to_string().contains("I like tea"));
        // The replaced session has today's tools and its old conversation.
        assert_eq!(offered_tools(&requests[2]).len(), 6);
        assert!(requests[2].to_string().contains("an earlier question"));
    }

    // Alice again: what her agent saved a turn ago is now in her context,
    // and her conversation continues.
    assert_eq!(
        ask(ALICE, "what do I like?"),
        (ALICE, "You like tea.".to_owned())
    );
    {
        let requests = requests.lock().unwrap();
        let prompt = system_prompt(&requests[3]);
        assert!(prompt.contains("alice likes tea"), "{prompt}");
        assert!(!prompt.contains("bob likes coffee"), "{prompt}");
        assert!(requests[3].to_string().contains("I like tea"));
    }

    // Alice asks for a reminder; her agent's schedule_add lands in her tasks.
    assert_eq!(
        ask(ALICE, "remind me to call mum on new year's day 2099"),
        (ALICE, "Scheduled.".to_owned())
    );
    let schedule = ScheduleStore::new(&config.workspace);
    let tasks = schedule.list("alice").unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].when, When::At("2099-01-01 09:00".into()));
    assert_eq!(tasks[0].prompt, "call mum");
    assert!(schedule.list("bob").unwrap().is_empty());

    // Heartbeats. Alice has a checklist. The first heartbeat's answer is
    // HEARTBEAT_OK, so nothing is sent; the next message to arrive is the
    // second heartbeat's.
    write(
        &workspace.join("memory/personal/alice/HEARTBEAT.md"),
        "- tell me about the tides\n",
    );
    dispatcher.enqueue("alice", Job::Heartbeat);
    dispatcher.enqueue("alice", Job::Heartbeat);
    assert_eq!(
        replies.recv_timeout(REPLY_TIMEOUT).unwrap(),
        (ALICE, "High tide is at six.".to_owned())
    );
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 8);
        let heartbeat = requests[6].to_string();
        assert!(
            heartbeat.contains("not a message from alice"),
            "{heartbeat}"
        );
        assert!(heartbeat.contains("tell me about the tides"), "{heartbeat}");
    }

    // A task that has come due runs in Alice's session and reaches her.
    dispatcher.enqueue("alice", Job::Scheduled(tasks[0].clone()));
    assert_eq!(
        replies.recv_timeout(REPLY_TIMEOUT).unwrap(),
        (ALICE, "Time to call mum.".to_owned())
    );
    {
        let requests = requests.lock().unwrap();
        let task = requests[8].to_string();
        assert!(task.contains("Scheduled task 1"), "{task}");
        assert!(task.contains("call mum"), "{task}");
    }

    // Bob has no checklist, so his heartbeat asks the model nothing: the
    // next request is his own message.
    dispatcher.enqueue("bob", Job::Heartbeat);
    assert_eq!(
        ask(BOB, "still there?"),
        (BOB, "Still here, Bob.".to_owned())
    );
    assert_eq!(requests.lock().unwrap().len(), 10);

    // Stop the daemon the way a service manager would.
    // SAFETY: kill(2) has no memory-safety preconditions; the daemon in this
    // process handles SIGTERM as a shutdown request.
    let signalled = unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
    assert_eq!(signalled, 0);
    for _ in 0..400 {
        if daemon.is_finished() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(daemon.is_finished(), "the daemon did not shut down");
    daemon.join().unwrap().unwrap();
}
