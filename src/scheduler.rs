//! The clock: decides when each user's heartbeat and scheduled tasks run,
//! and queues them with the user's other work.

use crate::gateway::{Dispatcher, Job, Transport};
use crate::heartbeat::ActiveHours;
use crate::schedule::{ScheduleStore, local_now};
use chrono::{Duration, NaiveDateTime};
use std::collections::HashMap;
use std::sync::Arc;

/// How often the clock is checked. Tasks are scheduled to the minute.
const TICK: std::time::Duration = std::time::Duration::from_secs(20);
/// How far back a tick looks for repeating tasks it missed, for example
/// while the machine slept. Older firings are skipped, not run late in a
/// burst. One-off tasks are never skipped.
const CATCH_UP_MINUTES: i64 = 5;

/// What to run for which user, decided from the time alone.
pub struct Schedule {
    users: Vec<String>,
    heartbeat: Option<(Duration, Option<ActiveHours>)>,
    next_heartbeat: HashMap<String, NaiveDateTime>,
    /// The last minute tasks were checked up to.
    checked: NaiveDateTime,
}

impl Schedule {
    pub fn new(config: &crate::config::Config, now: NaiveDateTime) -> Self {
        let heartbeat = config.heartbeat.as_ref().map(|heartbeat| {
            (
                Duration::minutes(i64::from(heartbeat.every_minutes)),
                // Validated when the configuration was loaded.
                heartbeat.active_hours().ok().flatten(),
            )
        });
        let users: Vec<String> = config.users.iter().map(|user| user.name.clone()).collect();
        let next_heartbeat = match &heartbeat {
            Some((every, _)) => users
                .iter()
                .map(|user| (user.clone(), now + *every))
                .collect(),
            None => HashMap::new(),
        };
        Self {
            users,
            heartbeat,
            next_heartbeat,
            checked: now,
        }
    }

    /// The jobs that have come due by `now`. `is_idle` says whether a user
    /// has nothing queued or running: a heartbeat for a busy user is
    /// skipped, since they are in the middle of something.
    pub fn tick(
        &mut self,
        now: NaiveDateTime,
        store: &ScheduleStore,
        is_idle: impl Fn(&str) -> bool,
    ) -> Vec<(String, Job)> {
        let mut jobs = Vec::new();
        let after = self.checked.max(now - Duration::minutes(CATCH_UP_MINUTES));
        for user in &self.users {
            match store.take_due(user, after, now) {
                Ok(due) => {
                    jobs.extend(
                        due.into_iter()
                            .map(|task| (user.clone(), Job::Scheduled(task))),
                    );
                }
                Err(error) => eprintln!("gateway: cannot read {user}'s schedule: {error:#}"),
            }
        }
        self.checked = self.checked.max(now);
        if let Some((every, hours)) = &self.heartbeat {
            for user in &self.users {
                let next = self
                    .next_heartbeat
                    .entry(user.clone())
                    .or_insert(now + *every);
                if now < *next {
                    continue;
                }
                *next = now + *every;
                let active = hours.is_none_or(|hours| hours.contains(now));
                if active && is_idle(user) {
                    jobs.push((user.clone(), Job::Heartbeat));
                }
            }
        }
        jobs
    }
}

/// Check the clock until the process ends.
pub async fn run<T: Transport>(dispatcher: Arc<Dispatcher<T>>, store: Arc<ScheduleStore>) {
    let mut schedule = Schedule::new(dispatcher.config(), local_now());
    loop {
        tokio::time::sleep(TICK).await;
        let jobs = schedule.tick(local_now(), &store, |user| dispatcher.is_idle(user));
        for (user, job) in jobs {
            dispatcher.enqueue(&user, job);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Heartbeat, Model, User};
    use crate::schedule::When;
    use chrono::NaiveDate;
    use tempfile::TempDir;

    fn at(hour: u32, minute: u32) -> NaiveDateTime {
        // Monday 23 November 2026.
        NaiveDate::from_ymd_opt(2026, 11, 23)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    fn config(heartbeat: Option<Heartbeat>) -> Config {
        Config {
            workspace: "/w".into(),
            state_directory: "/s".into(),
            xshell_config: None,
            model: Model {
                profile_name: None,
                provider: "ollama".into(),
                model: "m".into(),
                base_url: "http://localhost".into(),
                api_key_env: None,
                max_history_bytes: None,
                context_tokens: None,
                reserve_output_tokens: None,
            },
            telegram: None,
            web_search: None,
            heartbeat,
            users: ["alice", "bob"]
                .map(|name| User {
                    name: name.into(),
                    telegram_id: None,
                    tools: None,
                    shell: None,
                })
                .to_vec(),
        }
    }

    fn kinds(jobs: &[(String, Job)]) -> Vec<(&str, &str)> {
        jobs.iter()
            .map(|(user, job)| {
                let kind = match job {
                    Job::Heartbeat => "heartbeat",
                    Job::Scheduled(_) => "task",
                    Job::Chat(_) => "chat",
                };
                (user.as_str(), kind)
            })
            .collect()
    }

    #[test]
    fn heartbeats_come_round_per_user_skip_busy_users_and_keep_to_active_hours() {
        let temporary = TempDir::new().unwrap();
        let store = ScheduleStore::new(temporary.path());
        let mut schedule = Schedule::new(
            &config(Some(Heartbeat {
                every_minutes: 30,
                active_hours: Some("08:00-22:00".into()),
            })),
            at(21, 0),
        );
        // Not due yet.
        assert!(schedule.tick(at(21, 29), &store, |_| true).is_empty());
        // Due: Bob is busy, so only Alice gets one.
        let jobs = schedule.tick(at(21, 30), &store, |user| user != "bob");
        assert_eq!(kinds(&jobs), [("alice", "heartbeat")]);
        // The next round, at 22:00, is the last inside active hours.
        assert!(schedule.tick(at(21, 45), &store, |_| true).is_empty());
        assert_eq!(schedule.tick(at(22, 0), &store, |_| true).len(), 2);
        // 22:30 is outside them: skipped, and not made up for later.
        assert!(schedule.tick(at(22, 30), &store, |_| true).is_empty());
        assert!(schedule.tick(at(22, 31), &store, |_| true).is_empty());
    }

    #[test]
    fn without_a_heartbeat_section_there_are_none() {
        let temporary = TempDir::new().unwrap();
        let store = ScheduleStore::new(temporary.path());
        let mut schedule = Schedule::new(&config(None), at(9, 0));
        assert!(schedule.tick(at(23, 0), &store, |_| true).is_empty());
    }

    #[test]
    fn due_tasks_are_queued_once_for_their_own_user_even_when_busy() {
        let temporary = TempDir::new().unwrap();
        let store = ScheduleStore::new(temporary.path());
        store
            .add("bob", When::Cron("0 9 * * *".into()), "stand-up", at(8, 0))
            .unwrap();
        let mut schedule = Schedule::new(&config(None), at(8, 58));
        assert!(schedule.tick(at(8, 59), &store, |_| false).is_empty());
        let jobs = schedule.tick(at(9, 0), &store, |_| false);
        assert_eq!(kinds(&jobs), [("bob", "task")]);
        // Later ticks in and after that minute do not run it again.
        assert!(schedule.tick(at(9, 0), &store, |_| false).is_empty());
        assert!(schedule.tick(at(9, 1), &store, |_| false).is_empty());
    }

    #[test]
    fn a_long_gap_runs_missed_one_off_tasks_but_not_old_repeats() {
        let temporary = TempDir::new().unwrap();
        let store = ScheduleStore::new(temporary.path());
        store
            .add("alice", When::Cron("0 9 * * *".into()), "repeat", at(8, 0))
            .unwrap();
        store
            .add(
                "alice",
                When::At("2026-11-23 10:00".into()),
                "once",
                at(8, 0),
            )
            .unwrap();
        let mut schedule = Schedule::new(&config(None), at(8, 30));
        // The machine slept from 08:30 to 14:00.
        let jobs = schedule.tick(at(14, 0), &store, |_| true);
        let prompts: Vec<_> = jobs
            .iter()
            .map(|(_, job)| match job {
                Job::Scheduled(task) => task.prompt.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(prompts, ["once"]);
    }
}
