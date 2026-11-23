//! Heartbeats: periodic check-ins in which the agent decides whether
//! anything needs its user's attention.
//!
//! On each heartbeat the gateway runs a turn in the user's session with
//! their checklist: the operator's `HEARTBEAT.md` at the top of the
//! workspace, and the user's own `HEARTBEAT.md` in their memory, which
//! their agent can edit with `memory_write`. If nothing needs attention the
//! agent answers `HEARTBEAT_OK` and the user hears nothing.
//!
//! A user with no checklist gets no heartbeat: there would be nothing to
//! check, and each one costs a model turn.

use crate::workspace::{Scope, Workspace};
use anyhow::{Context, Result, bail};
use chrono::{NaiveDateTime, NaiveTime};

/// What the agent answers when nothing needs attention.
pub const OK_TOKEN: &str = "HEARTBEAT_OK";
/// With the token, an answer this short is an acknowledgement and is not
/// sent. A longer one is sent, in case the model added the token to a real
/// message.
const MAX_ACK_CHARS: usize = 300;
/// Most of one checklist file that goes into the heartbeat's message.
const MAX_CHECKLIST_BYTES: usize = 16 * 1024;

/// Hours of the day, local time, in which heartbeats may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveHours {
    start: NaiveTime,
    end: NaiveTime,
}

impl ActiveHours {
    /// Parse `HH:MM-HH:MM`. A window that ends before it starts wraps past
    /// midnight (`22:00-06:00`).
    pub fn parse(text: &str) -> Result<Self> {
        let (start, end) = text
            .split_once('-')
            .with_context(|| format!("active hours {text:?} must be written HH:MM-HH:MM"))?;
        let time = |part: &str| {
            let part = part.trim();
            // "24:00" is a natural way to write the end of the day.
            if part == "24:00" {
                return Ok(NaiveTime::from_hms_opt(23, 59, 59).expect("a valid time"));
            }
            NaiveTime::parse_from_str(part, "%H:%M")
                .with_context(|| format!("{part:?} in active hours {text:?} is not a time HH:MM"))
        };
        let (start, end) = (time(start)?, time(end)?);
        if start == end {
            bail!("active hours {text:?} cover no time");
        }
        Ok(Self { start, end })
    }

    pub fn contains(&self, now: NaiveDateTime) -> bool {
        let time = now.time();
        if self.start < self.end {
            self.start <= time && time <= self.end
        } else {
            time >= self.start || time <= self.end
        }
    }
}

/// The message that starts `user`'s heartbeat turn, or `None` when there is
/// no checklist to act on.
pub fn prompt(workspace: &Workspace, user: &str) -> Result<Option<String>> {
    let sections: Vec<String> = [
        (
            "Everyone's checklist",
            workspace.operator_file("HEARTBEAT.md")?,
        ),
        (
            "This user's checklist (HEARTBEAT.md in their memory; you can edit it)",
            workspace.read_memory(Scope::Personal(user), "HEARTBEAT.md")?,
        ),
    ]
    .into_iter()
    .filter_map(|(title, text)| {
        let text = bounded(text?.trim());
        (!text.is_empty()).then(|| format!("## {title}\n\n{text}"))
    })
    .collect();
    if sections.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "[Heartbeat. This is a scheduled check-in, not a message from {user}.]\n\n\
Go through the checklist below. If something needs {user}'s attention now, write the message \
to send them and nothing else. If nothing does, answer with exactly {OK_TOKEN}. Do not repeat \
things you have already told them unless the checklist says to.\n\n{}",
        sections.join("\n\n")
    )))
}

/// What to send the user for a heartbeat's answer: `None` when the agent
/// reported that nothing needs attention.
pub fn visible(answer: &str) -> Option<String> {
    let trimmed = answer.trim();
    let rest = trimmed
        .strip_prefix(OK_TOKEN)
        .or_else(|| trimmed.strip_suffix(OK_TOKEN));
    match rest {
        Some(rest) if rest.trim().chars().count() <= MAX_ACK_CHARS => None,
        // The token came with a real message: send the message without it.
        Some(rest) => Some(rest.trim().to_owned()),
        None if trimmed.is_empty() => None,
        None => Some(trimmed.to_owned()),
    }
}

fn bounded(text: &str) -> String {
    if text.len() <= MAX_CHECKLIST_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_CHECKLIST_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[the rest of this file is not shown]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use tempfile::TempDir;

    fn at(hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 23)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    #[test]
    fn active_hours_cover_a_daytime_window_or_wrap_past_midnight() {
        let day = ActiveHours::parse("08:00-22:00").unwrap();
        assert!(!day.contains(at(7, 59)));
        assert!(day.contains(at(8, 0)));
        assert!(day.contains(at(22, 0)));
        assert!(!day.contains(at(22, 1)));
        let night = ActiveHours::parse("22:00-06:00").unwrap();
        assert!(night.contains(at(23, 0)));
        assert!(night.contains(at(5, 59)));
        assert!(!night.contains(at(12, 0)));
        assert!(
            ActiveHours::parse("08:00-24:00")
                .unwrap()
                .contains(at(23, 59))
        );
        for bad in ["08:00", "8am-10pm", "08:00-08:00", "25:00-26:00"] {
            assert!(ActiveHours::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_ok_answer_is_silent_and_anything_else_is_sent() {
        for silent in [
            "HEARTBEAT_OK",
            "  HEARTBEAT_OK\n",
            "HEARTBEAT_OK Nothing due today.",
            "All quiet. HEARTBEAT_OK",
            "",
        ] {
            assert_eq!(visible(silent), None, "{silent:?}");
        }
        assert_eq!(
            visible("Your passport expires in 3 weeks.").as_deref(),
            Some("Your passport expires in 3 weeks.")
        );
        // The token in the middle of a message is just text.
        assert_eq!(
            visible("I would say HEARTBEAT_OK but the bins are due.").as_deref(),
            Some("I would say HEARTBEAT_OK but the bins are due.")
        );
        // The token with a long message: the message is real.
        let long = format!("{OK_TOKEN} {}", "the boiler is leaking ".repeat(30));
        let sent = visible(&long).unwrap();
        assert!(sent.starts_with("the boiler is leaking"));
        assert!(!sent.contains(OK_TOKEN));
    }

    #[test]
    fn the_prompt_carries_both_checklists_and_is_absent_without_one() {
        let temporary = TempDir::new().unwrap();
        let workspace = Workspace::new(temporary.path().to_owned());
        assert_eq!(prompt(&workspace, "alice").unwrap(), None);
        // An empty file is no checklist either.
        std::fs::write(temporary.path().join("HEARTBEAT.md"), "\n").unwrap();
        assert_eq!(prompt(&workspace, "alice").unwrap(), None);

        workspace
            .write_memory(
                Scope::Personal("alice"),
                "HEARTBEAT.md",
                "- check the tide tables",
                true,
            )
            .unwrap();
        let alice = prompt(&workspace, "alice").unwrap().unwrap();
        assert!(alice.contains("not a message from alice"));
        assert!(alice.contains("check the tide tables"));
        assert!(alice.contains(OK_TOKEN));
        // Bob has no checklist of his own and the shared one is empty.
        assert_eq!(prompt(&workspace, "bob").unwrap(), None);

        std::fs::write(temporary.path().join("HEARTBEAT.md"), "- bins on Tuesday\n").unwrap();
        let bob = prompt(&workspace, "bob").unwrap().unwrap();
        assert!(bob.contains("bins on Tuesday"));
        assert!(!bob.contains("tide tables"));
    }
}
