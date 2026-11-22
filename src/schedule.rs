//! Scheduled tasks: prompts a user's agent runs later, once or on a
//! repeating schedule.
//!
//! A task belongs to one user. It is created, listed and removed by that
//! user's agent through the schedule tools, usually because the user asked
//! ("remind me every Monday at nine"). When a task is due the gateway runs
//! its prompt as a turn in the user's own session and sends them the answer.
//!
//! Tasks are kept in `schedule/<user>.json` in the workspace. All times are
//! in the gateway's local time zone.

use crate::cron::Cron;
use crate::directory::Directory;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{NaiveDateTime, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use xshell_core::ToolDefinition;
use xshell_execution::{ProvidedTool, ToolAccess, ToolApproval, ToolContext, ToolProvider};

/// The schedule tools a chat session is offered.
pub const TOOL_NAMES: [&str; 3] = ["schedule_add", "schedule_list", "schedule_remove"];

/// Most tasks one user may have.
const MAX_TASKS: usize = 50;
/// Longest task prompt.
const MAX_PROMPT_CHARS: usize = 2000;
/// Most times a repeating task may fire in a day (every 15 minutes). Each
/// firing is a model turn, so this bounds what one task can cost.
const MAX_FIRES_PER_DAY: u32 = 96;
/// How one-off times are written and read.
const AT_FORMAT: &str = "%Y-%m-%d %H:%M";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub when: When,
    pub prompt: String,
    /// When the task was created, for the user's reference.
    pub created: String,
}

/// When a task runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum When {
    /// Repeatedly, on a five-field cron expression.
    Cron(String),
    /// Once, at `YYYY-MM-DD HH:MM`; the task is removed when it runs.
    At(String),
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserTasks {
    next_id: u64,
    tasks: Vec<Task>,
}

/// Every user's tasks, on disk.
#[derive(Debug)]
pub struct ScheduleStore {
    directory: PathBuf,
    /// One writer at a time: the tools and the scheduler share the files.
    lock: Mutex<()>,
}

impl ScheduleStore {
    /// A store under `workspace`.
    pub fn new(workspace: &std::path::Path) -> Self {
        Self {
            directory: workspace.join("schedule"),
            lock: Mutex::new(()),
        }
    }

    pub fn list(&self, user: &str) -> Result<Vec<Task>> {
        let _guard = self.guard();
        Ok(self.load(user)?.tasks)
    }

    /// Add a task for `user`, checked against the limits.
    pub fn add(&self, user: &str, when: When, prompt: &str, now: NaiveDateTime) -> Result<Task> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            bail!("a task needs a prompt: what should be done when it runs");
        }
        if prompt.chars().count() > MAX_PROMPT_CHARS {
            bail!("the prompt is longer than {MAX_PROMPT_CHARS} characters");
        }
        match &when {
            When::Cron(expression) => {
                let cron = Cron::parse(expression)?;
                if cron.fires_per_day() > MAX_FIRES_PER_DAY {
                    bail!(
                        "that schedule can run {} times a day; the most allowed is \
{MAX_FIRES_PER_DAY} (every 15 minutes)",
                        cron.fires_per_day()
                    );
                }
                if cron.next_after(now).is_none() {
                    bail!("that schedule never runs");
                }
            }
            When::At(time) => {
                if parse_at(time)? <= now {
                    bail!(
                        "{time} is not in the future; it is now {}",
                        now.format(AT_FORMAT)
                    );
                }
            }
        }
        let _guard = self.guard();
        let mut tasks = self.load(user)?;
        if tasks.tasks.len() >= MAX_TASKS {
            bail!("there are already {MAX_TASKS} tasks; remove one first");
        }
        tasks.next_id += 1;
        let task = Task {
            id: tasks.next_id.to_string(),
            when,
            prompt: prompt.to_owned(),
            created: now.format(AT_FORMAT).to_string(),
        };
        tasks.tasks.push(task.clone());
        self.save(user, &tasks)?;
        Ok(task)
    }

    /// Remove a task. Returns whether there was one with that id.
    pub fn remove(&self, user: &str, id: &str) -> Result<bool> {
        let _guard = self.guard();
        let mut tasks = self.load(user)?;
        let before = tasks.tasks.len();
        tasks.tasks.retain(|task| task.id != id);
        if tasks.tasks.len() == before {
            return Ok(false);
        }
        self.save(user, &tasks)?;
        Ok(true)
    }

    /// The tasks due in the minutes after `after`, up to and including
    /// `until`. A one-off task is due once its time has come, however long
    /// ago, and is removed here, so it runs exactly once.
    pub fn take_due(
        &self,
        user: &str,
        after: NaiveDateTime,
        until: NaiveDateTime,
    ) -> Result<Vec<Task>> {
        let _guard = self.guard();
        let mut tasks = self.load(user)?;
        let mut due = Vec::new();
        let mut spent = Vec::new();
        for task in &tasks.tasks {
            match &task.when {
                When::Cron(expression) => {
                    // A stored expression was valid when added; skip one
                    // that no longer parses instead of stopping the rest.
                    let fires = Cron::parse(expression)
                        .ok()
                        .and_then(|cron| cron.next_after(after))
                        .is_some_and(|next| next <= until);
                    if fires {
                        due.push(task.clone());
                    }
                }
                When::At(time) => {
                    if parse_at(time).is_ok_and(|time| time <= until) {
                        due.push(task.clone());
                        spent.push(task.id.clone());
                    }
                }
            }
        }
        if !spent.is_empty() {
            tasks.tasks.retain(|task| !spent.contains(&task.id));
            self.save(user, &tasks)?;
        }
        Ok(due)
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn file(&self, user: &str) -> Result<PathBuf> {
        crate::config::validate_user_name(user)?;
        Ok(self.directory.join(format!("{user}.json")))
    }

    fn load(&self, user: &str) -> Result<UserTasks> {
        let file = self.file(user)?;
        match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not a valid task file", file.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(UserTasks::default()),
            Err(error) => Err(error).with_context(|| format!("cannot read {}", file.display())),
        }
    }

    fn save(&self, user: &str, tasks: &UserTasks) -> Result<()> {
        let file = self.file(user)?;
        std::fs::create_dir_all(&self.directory)
            .with_context(|| format!("cannot create {}", self.directory.display()))?;
        let temporary = file.with_extension("json.new");
        let mut handle = std::fs::File::create(&temporary)
            .with_context(|| format!("cannot write {}", temporary.display()))?;
        handle.write_all(&serde_json::to_vec_pretty(tasks)?)?;
        handle.sync_all()?;
        std::fs::rename(&temporary, &file)
            .with_context(|| format!("cannot replace {}", file.display()))?;
        Ok(())
    }
}

fn parse_at(time: &str) -> Result<NaiveDateTime> {
    NaiveDateTime::parse_from_str(time.trim(), AT_FORMAT)
        .with_context(|| format!("{time:?} is not a time written as YYYY-MM-DD HH:MM"))
}

/// One line describing a task and when it next runs.
fn describe(task: &Task, now: NaiveDateTime) -> String {
    let (kind, next) = match &task.when {
        When::Cron(expression) => (
            format!("repeats \"{expression}\""),
            Cron::parse(expression)
                .ok()
                .and_then(|cron| cron.next_after(now))
                .map(|next| next.format(AT_FORMAT).to_string()),
        ),
        When::At(time) => ("once".to_owned(), Some(time.clone())),
    };
    format!(
        "{}: {kind}, next {}: {}",
        task.id,
        next.as_deref().unwrap_or("never"),
        task.prompt
    )
}

/// The current local time, to the minute.
pub fn local_now() -> NaiveDateTime {
    let now = chrono::Local::now().naive_local();
    now.with_second(0)
        .and_then(|now| now.with_nanosecond(0))
        .unwrap_or(now)
}

/// The schedule tools.
pub struct ScheduleProvider {
    store: Arc<ScheduleStore>,
    directory: Arc<Directory>,
}

impl ScheduleProvider {
    pub fn new(store: Arc<ScheduleStore>, directory: Arc<Directory>) -> Self {
        Self { store, directory }
    }
}

#[async_trait]
impl ToolProvider for ScheduleProvider {
    fn name(&self) -> &str {
        "schedule"
    }

    fn tools(&self) -> Vec<ProvidedTool> {
        vec![
            ProvidedTool {
                definition: ToolDefinition {
                    name: "schedule_add".into(),
                    description: "Schedule something for later: a reminder, or a task to do and \
report on. When it is due, you are run with the prompt and your answer is sent to this user. \
Give exactly one of `cron` (repeating) or `at` (once). Times are local; the current time is \
in your instructions."
                        .into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "cron": {
                                "type": "string",
                                "description": "Five fields: minute hour day-of-month month \
                    day-of-week (0 or 7 is Sunday). Numbers, ranges, lists and steps only. Example: \
                    \"0 9 * * 1-5\" is 09:00 on weekdays. At most every 15 minutes"
                            },
                            "at": {
                                "type": "string",
                                "description": "One time, as YYYY-MM-DD HH:MM, in the future"
                            },
                            "prompt": {
                                "type": "string",
                                "description": "What to do when it runs, written so it makes \
                    sense on its own later. For a reminder, say what to remind the user of"
                            }
                        },
                        "required": ["prompt"],
                        "additionalProperties": false
                    }),
                },
                access: ToolAccess::ReadWrite,
                approval: ToolApproval::NotRequired,
            },
            ProvidedTool {
                definition: ToolDefinition {
                    name: "schedule_list".into(),
                    description: "List this user's scheduled tasks with their ids and next run \
times."
                        .into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }),
                },
                access: ToolAccess::ReadOnly,
                approval: ToolApproval::NotRequired,
            },
            ProvidedTool {
                definition: ToolDefinition {
                    name: "schedule_remove".into(),
                    description: "Remove one of this user's scheduled tasks by its id.".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "The id from schedule_list"}
                        },
                        "required": ["id"],
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
            .context("this session has no user, so it has no schedule")?;
        let store = Arc::clone(&self.store);
        let tool = tool.to_owned();
        tokio::task::spawn_blocking(move || {
            run(&store, &user, &tool, &context.arguments, local_now())
        })
        .await
        .context("the schedule tool stopped unexpectedly")?
    }
}

fn run(
    store: &ScheduleStore,
    user: &str,
    tool: &str,
    arguments: &Map<String, Value>,
    now: NaiveDateTime,
) -> Result<String> {
    let text = |key: &str| match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(anyhow::anyhow!("\"{key}\" must be a string")),
    };
    match tool {
        "schedule_add" => {
            let when = match (text("cron")?, text("at")?) {
                (Some(cron), None) => When::Cron(cron.trim().to_owned()),
                (None, Some(at)) => When::At(at.trim().to_owned()),
                _ => bail!("give exactly one of \"cron\" or \"at\""),
            };
            let prompt = text("prompt")?.context("schedule_add needs a \"prompt\"")?;
            let task = store.add(user, when, prompt, now)?;
            Ok(format!("scheduled. {}", describe(&task, now)))
        }
        "schedule_list" => {
            let tasks = store.list(user)?;
            if tasks.is_empty() {
                return Ok("no scheduled tasks".into());
            }
            Ok(tasks
                .iter()
                .map(|task| describe(task, now))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "schedule_remove" => {
            let id = text("id")?.context("schedule_remove needs an \"id\"")?;
            if store.remove(user, id.trim())? {
                Ok(format!("removed task {}", id.trim()))
            } else {
                bail!("there is no task with id {:?}", id.trim())
            }
        }
        other => bail!("unknown schedule tool {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use tempfile::TempDir;

    fn at(day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        // November 2026; the 23rd is a Monday.
        NaiveDate::from_ymd_opt(2026, 11, day)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    fn store() -> (TempDir, ScheduleStore) {
        let temporary = TempDir::new().unwrap();
        let store = ScheduleStore::new(temporary.path());
        (temporary, store)
    }

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn a_repeating_task_is_due_in_each_window_its_time_falls_in() {
        let (_temporary, store) = store();
        let now = at(23, 8, 0);
        store
            .add("alice", When::Cron("0 9 * * 1-5".into()), "stand-up", now)
            .unwrap();
        // Windows are (after, until].
        assert!(
            store
                .take_due("alice", at(23, 8, 0), at(23, 8, 59))
                .unwrap()
                .is_empty()
        );
        let due = store
            .take_due("alice", at(23, 8, 59), at(23, 9, 0))
            .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].prompt, "stand-up");
        assert!(
            store
                .take_due("alice", at(23, 9, 0), at(23, 9, 1))
                .unwrap()
                .is_empty()
        );
        // Still there for Tuesday, and not due on Saturday.
        assert_eq!(
            store
                .take_due("alice", at(24, 8, 59), at(24, 9, 0))
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .take_due("alice", at(28, 8, 59), at(28, 9, 0))
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.list("alice").unwrap().len(), 1);
    }

    #[test]
    fn a_one_off_task_runs_once_even_if_it_came_due_while_nothing_was_running() {
        let (_temporary, store) = store();
        store
            .add(
                "alice",
                When::At("2026-11-23 09:30".into()),
                "call mum",
                at(23, 8, 0),
            )
            .unwrap();
        assert!(
            store
                .take_due("alice", at(23, 9, 0), at(23, 9, 29))
                .unwrap()
                .is_empty()
        );
        // The gateway was down at 09:30 and looks again in the afternoon.
        let due = store
            .take_due("alice", at(23, 14, 0), at(23, 14, 1))
            .unwrap();
        assert_eq!(due.len(), 1);
        assert!(
            store
                .take_due("alice", at(23, 14, 1), at(23, 14, 2))
                .unwrap()
                .is_empty()
        );
        assert!(store.list("alice").unwrap().is_empty());
    }

    #[test]
    fn tasks_are_per_user_and_survive_a_new_store() {
        let (temporary, store) = store();
        let now = at(23, 8, 0);
        store
            .add("alice", When::Cron("0 9 * * *".into()), "a", now)
            .unwrap();
        store
            .add("bob", When::Cron("0 9 * * *".into()), "b", now)
            .unwrap();
        let reopened = ScheduleStore::new(temporary.path());
        let alice = reopened.list("alice").unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].prompt, "a");
        assert!(!reopened.remove("alice", "99").unwrap());
        assert!(reopened.remove("alice", &alice[0].id).unwrap());
        assert!(reopened.list("alice").unwrap().is_empty());
        assert_eq!(reopened.list("bob").unwrap().len(), 1);
        // Ids are not reused after a removal.
        let again = reopened
            .add("alice", When::Cron("0 9 * * *".into()), "c", now)
            .unwrap();
        assert_ne!(again.id, alice[0].id);
        assert!(reopened.list("../bob").is_err());
    }

    #[test]
    fn tasks_that_are_malformed_too_frequent_or_in_the_past_are_refused() {
        let (_temporary, store) = store();
        let now = at(23, 8, 0);
        for (when, prompt, message) in [
            (When::Cron("* * * * *".into()), "x", "1440 times a day"),
            (When::Cron("*/10 * * * *".into()), "x", "144 times a day"),
            (When::Cron("0 9 * *".into()), "x", "five fields"),
            (When::Cron("0 0 31 2 *".into()), "x", "never runs"),
            (
                When::At("2026-11-23 07:59".into()),
                "x",
                "not in the future",
            ),
            (When::At("tomorrow at nine".into()), "x", "YYYY-MM-DD HH:MM"),
            (When::Cron("0 9 * * *".into()), "  ", "needs a prompt"),
        ] {
            let error = store.add("alice", when, prompt, now).unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
        let long = "x".repeat(MAX_PROMPT_CHARS + 1);
        assert!(
            store
                .add("alice", When::Cron("0 9 * * *".into()), &long, now)
                .is_err()
        );
        assert!(store.list("alice").unwrap().is_empty());
        for _ in 0..MAX_TASKS {
            store
                .add("alice", When::Cron("*/15 * * * *".into()), "x", now)
                .unwrap();
        }
        let error = store
            .add("alice", When::Cron("0 9 * * *".into()), "x", now)
            .unwrap_err();
        assert!(error.to_string().contains("already 50 tasks"));
    }

    #[test]
    fn the_tools_add_list_and_remove_a_users_tasks() {
        let (_temporary, store) = store();
        let now = at(23, 8, 0);
        let added = run(
            &store,
            "alice",
            "schedule_add",
            &arguments(json!({"cron": "30 8 * * 1", "prompt": "weekly review"})),
            now,
        )
        .unwrap();
        assert_eq!(
            added,
            "scheduled. 1: repeats \"30 8 * * 1\", next 2026-11-23 08:30: weekly review"
        );
        run(
            &store,
            "alice",
            "schedule_add",
            &arguments(json!({"at": "2026-11-24 17:00", "prompt": "call mum"})),
            now,
        )
        .unwrap();
        let listed = run(&store, "alice", "schedule_list", &Map::new(), now).unwrap();
        assert_eq!(
            listed,
            "1: repeats \"30 8 * * 1\", next 2026-11-23 08:30: weekly review\n\
2: once, next 2026-11-24 17:00: call mum"
        );
        assert_eq!(
            run(&store, "bob", "schedule_list", &Map::new(), now).unwrap(),
            "no scheduled tasks"
        );
        assert_eq!(
            run(
                &store,
                "alice",
                "schedule_remove",
                &arguments(json!({"id": "1"})),
                now
            )
            .unwrap(),
            "removed task 1"
        );
        for (tool, value, message) in [
            ("schedule_remove", json!({"id": "1"}), "no task with id"),
            ("schedule_add", json!({"prompt": "x"}), "exactly one of"),
            (
                "schedule_add",
                json!({"cron": "0 9 * * *", "at": "2026-11-24 17:00", "prompt": "x"}),
                "exactly one of",
            ),
            (
                "schedule_add",
                json!({"cron": "0 9 * * *"}),
                "needs a \"prompt\"",
            ),
            (
                "schedule_add",
                json!({"cron": 9, "prompt": "x"}),
                "must be a string",
            ),
        ] {
            let error = run(&store, "alice", tool, &arguments(value), now).unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
    }
}
