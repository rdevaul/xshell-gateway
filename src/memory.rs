//! The memory tools: how an agent reads and writes its user's memory.
//!
//! Each call arrives with the daemon's session id. The user is looked up in
//! the [`Directory`], never taken from the model's arguments, so an agent
//! cannot name another user. A user's agent can read and write that user's
//! memory and read the shared memory; nothing here writes shared memory.

use crate::directory::Directory;
use crate::workspace::{Scope, Workspace};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use xshell_core::ToolDefinition;
use xshell_execution::{ProvidedTool, ToolAccess, ToolApproval, ToolContext, ToolProvider};

/// The tools a chat session is limited to.
pub const TOOL_NAMES: [&str; 3] = ["memory_read", "memory_list", "memory_write"];

/// Most of a file one read returns; the engine truncates tool output at
/// 64 KiB, and this leaves room for the note that says so.
const MAX_READ_BYTES: usize = 60 * 1024;

pub struct MemoryProvider {
    workspace: Workspace,
    directory: Arc<Directory>,
}

impl MemoryProvider {
    pub fn new(workspace: Workspace, directory: Arc<Directory>) -> Self {
        Self {
            workspace,
            directory,
        }
    }
}

#[async_trait]
impl ToolProvider for MemoryProvider {
    fn name(&self) -> &str {
        "memory"
    }

    fn tools(&self) -> Vec<ProvidedTool> {
        let scope = json!({
            "type": "string",
            "enum": ["personal", "shared"],
            "description": "personal (default): this user's own memory. shared: memory every \
        user's conversations can read"
        });
        let path = json!({
            "type": "string",
            "description": "File within the memory, ending in .md, such as MEMORY.md or \
        topics/boat.md. Defaults to MEMORY.md"
        });
        vec![
            ProvidedTool {
                definition: ToolDefinition {
                    name: "memory_read".into(),
                    description: "Read one of this user's memory files, or a shared one.".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {"scope": scope, "path": path},
                        "additionalProperties": false
                    }),
                },
                access: ToolAccess::ReadOnly,
                approval: ToolApproval::NotRequired,
            },
            ProvidedTool {
                definition: ToolDefinition {
                    name: "memory_list".into(),
                    description: "List this user's memory files, or the shared ones.".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {"scope": scope},
                        "additionalProperties": false
                    }),
                },
                access: ToolAccess::ReadOnly,
                approval: ToolApproval::NotRequired,
            },
            ProvidedTool {
                definition: ToolDefinition {
                    name: "memory_write".into(),
                    description: "Save something to this user's memory so later conversations \
have it. Appends by default. Keep MEMORY.md short and put longer notes in topic files."
                        .into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "path": path,
                            "content": {"type": "string", "description": "Markdown to save"},
                            "mode": {
                                "type": "string",
                                "enum": ["append", "replace"],
                                "description": "append (default) adds to the file; replace \
                    overwrites it"
                            }
                        },
                        "required": ["content"],
                        "additionalProperties": false
                    }),
                },
                access: ToolAccess::ReadWrite,
                approval: ToolApproval::NotRequired,
            },
        ]
    }

    async fn call(&self, tool: &str, context: ToolContext) -> Result<String> {
        let user = context
            .session_id
            .as_deref()
            .and_then(|session| self.directory.user_for(session))
            .context("this session has no user, so it has no memory")?;
        let workspace = self.workspace.clone();
        let tool = tool.to_owned();
        // File access is blocking; keep it off the turn's async runtime.
        tokio::task::spawn_blocking(move || run(&workspace, &user, &tool, &context.arguments))
            .await
            .context("the memory tool stopped unexpectedly")?
    }
}

fn run(
    workspace: &Workspace,
    user: &str,
    tool: &str,
    arguments: &Map<String, Value>,
) -> Result<String> {
    let path = optional_string(arguments, "path")?.unwrap_or("MEMORY.md");
    match tool {
        "memory_read" => {
            let scope = scope(arguments, user)?;
            match workspace.read_memory(scope, path)? {
                Some(text) => Ok(bounded(text)),
                None => Ok(format!("{path} does not exist yet")),
            }
        }
        "memory_list" => {
            let files = workspace.list_memory(scope(arguments, user)?)?;
            if files.is_empty() {
                Ok("no memory files yet".into())
            } else {
                Ok(files.join("\n"))
            }
        }
        "memory_write" => {
            let content = optional_string(arguments, "content")?
                .filter(|content| !content.trim().is_empty())
                .context("memory_write needs non-empty \"content\"")?;
            let append = match optional_string(arguments, "mode")? {
                None | Some("append") => true,
                Some("replace") => false,
                Some(other) => bail!("\"mode\" must be \"append\" or \"replace\", not {other:?}"),
            };
            let size = workspace.write_memory(Scope::Personal(user), path, content, append)?;
            Ok(format!("saved; {path} is now {size} bytes"))
        }
        other => bail!("unknown memory tool {other}"),
    }
}

fn scope<'a>(arguments: &Map<String, Value>, user: &'a str) -> Result<Scope<'a>> {
    match optional_string(arguments, "scope")? {
        None | Some("personal") => Ok(Scope::Personal(user)),
        Some("shared") => Ok(Scope::Shared),
        Some(other) => bail!("\"scope\" must be \"personal\" or \"shared\", not {other:?}"),
    }
}

fn optional_string<'a>(arguments: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => bail!("\"{key}\" must be a string"),
    }
}

fn bounded(mut text: String) -> String {
    if text.len() <= MAX_READ_BYTES {
        return text;
    }
    let mut end = MAX_READ_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n[the rest of the file is not shown]");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use xshell_execution::{CancellationFlag, ToolRegistry};

    fn provider() -> (TempDir, MemoryProvider) {
        let temporary = TempDir::new().unwrap();
        let directory = Arc::new(Directory::default());
        directory.register("session-alice", "alice");
        directory.register("session-bob", "bob");
        let provider = MemoryProvider::new(Workspace::new(temporary.path().to_owned()), directory);
        (temporary, provider)
    }

    async fn call(
        provider: &MemoryProvider,
        session: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<String> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments must be an object");
        };
        provider
            .call(
                tool,
                ToolContext {
                    arguments,
                    cwd: "/".into(),
                    session_id: Some(session.into()),
                    agent: None,
                    call_id: "call".into(),
                    cancellation: CancellationFlag::default(),
                },
            )
            .await
    }

    #[test]
    fn the_registry_accepts_the_tools_and_they_match_the_session_selection() {
        let (_temporary, provider) = provider();
        let registry = ToolRegistry::builder()
            .provider(Arc::new(provider))
            .build()
            .unwrap();
        let names: Vec<_> = registry.tools().iter().map(|tool| tool.name()).collect();
        assert_eq!(names, TOOL_NAMES);
    }

    #[tokio::test]
    async fn a_user_reads_back_what_their_agent_saved() {
        let (_temporary, provider) = provider();
        let saved = call(
            &provider,
            "session-alice",
            "memory_write",
            json!({"content": "likes tea"}),
        )
        .await
        .unwrap();
        assert!(saved.starts_with("saved; MEMORY.md"), "{saved}");
        assert_eq!(
            call(&provider, "session-alice", "memory_read", json!({}))
                .await
                .unwrap(),
            "likes tea\n"
        );
        assert_eq!(
            call(&provider, "session-alice", "memory_list", json!({}))
                .await
                .unwrap(),
            "MEMORY.md"
        );
    }

    #[tokio::test]
    async fn the_user_comes_from_the_session_never_from_the_arguments() {
        let (_temporary, provider) = provider();
        call(
            &provider,
            "session-alice",
            "memory_write",
            json!({"content": "alice's secret"}),
        )
        .await
        .unwrap();
        // Bob's session sees nothing of Alice's, whatever path it asks for.
        assert_eq!(
            call(&provider, "session-bob", "memory_read", json!({}))
                .await
                .unwrap(),
            "MEMORY.md does not exist yet"
        );
        for path in ["../alice/MEMORY.md", "/memory/personal/alice/MEMORY.md"] {
            assert!(
                call(
                    &provider,
                    "session-bob",
                    "memory_read",
                    json!({"path": path})
                )
                .await
                .is_err(),
                "{path}"
            );
        }
        // A session the gateway did not create has no memory at all.
        let error = call(&provider, "someone-elses-session", "memory_read", json!({}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("has no user"));
    }

    #[tokio::test]
    async fn shared_memory_is_readable_but_not_writable() {
        let (temporary, provider) = provider();
        let shared = temporary.path().join("memory/shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("MEMORY.md"), "bins go out on Tuesday\n").unwrap();
        assert_eq!(
            call(
                &provider,
                "session-bob",
                "memory_read",
                json!({"scope": "shared"})
            )
            .await
            .unwrap(),
            "bins go out on Tuesday\n"
        );
        // memory_write takes no scope: with one, it still writes Bob's own.
        call(
            &provider,
            "session-bob",
            "memory_write",
            json!({"content": "mine", "scope": "shared"}),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(shared.join("MEMORY.md")).unwrap(),
            "bins go out on Tuesday\n"
        );
    }

    #[tokio::test]
    async fn bad_arguments_are_errors_the_model_can_read() {
        let (_temporary, provider) = provider();
        for (tool, arguments, message) in [
            ("memory_write", json!({}), "needs non-empty"),
            ("memory_write", json!({"content": "  "}), "needs non-empty"),
            (
                "memory_write",
                json!({"content": "x", "mode": "prepend"}),
                "must be \"append\" or \"replace\"",
            ),
            (
                "memory_read",
                json!({"scope": "bob"}),
                "must be \"personal\" or \"shared\"",
            ),
            ("memory_read", json!({"path": 7}), "must be a string"),
        ] {
            let error = call(&provider, "session-alice", tool, arguments)
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
    }
}
