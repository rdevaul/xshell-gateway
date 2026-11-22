//! The Telegram Bot API, as a [`Transport`].
//!
//! Long polling only: the gateway opens no listening port. Only private
//! chats are handled; a message in a group is ignored, because a group
//! would mix several people into one user's memory.
//!
//! The bot token is part of every request URL, so errors are reported
//! without their URL.

use crate::gateway::{Inbound, Transport, chunks};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

/// Telegram's limit on one message.
const MAX_MESSAGE_CHARS: usize = 4096;
/// How long Telegram holds a `getUpdates` request open.
const POLL_SECONDS: u64 = 25;

pub struct Telegram {
    client: reqwest::Client,
    base: String,
    /// The next update to ask for: one past the last update seen.
    offset: AtomicI64,
}

impl Telegram {
    /// A client for the bot whose token is in the environment variable
    /// `token_env`.
    pub fn from_env(token_env: &str) -> Result<Self> {
        let token = std::env::var(token_env)
            .ok()
            .filter(|token| !token.is_empty())
            .with_context(|| format!("the bot token variable {token_env} is not set"))?;
        Self::new("https://api.telegram.org", &token)
    }

    /// `api` is the service's base URL; tests point it at a local server.
    pub fn new(api: &str, token: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(POLL_SECONDS + 15))
            .build()
            .context("cannot build the HTTP client")?;
        Ok(Self {
            client,
            base: format!("{}/bot{token}", api.trim_end_matches('/')),
            offset: AtomicI64::new(0),
        })
    }

    async fn call<R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        body: serde_json::Value,
    ) -> Result<R> {
        let response = self
            .client
            .post(format!("{}/{method}", self.base))
            .json(&body)
            .send()
            .await
            // The URL holds the token; never let it into an error.
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("Telegram {method} failed"))?;
        let reply: Reply<R> = response
            .json()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("Telegram {method} sent an unreadable reply"))?;
        match reply {
            Reply {
                ok: true,
                result: Some(result),
                ..
            } => Ok(result),
            Reply { description, .. } => bail!(
                "Telegram {method} was refused: {}",
                description.unwrap_or_else(|| "no reason given".into())
            ),
        }
    }
}

#[async_trait]
impl Transport for Telegram {
    async fn receive(&self) -> Result<Vec<Inbound>> {
        let updates: Vec<Update> = self
            .call(
                "getUpdates",
                json!({
                    "offset": self.offset.load(Ordering::Acquire),
                    "timeout": POLL_SECONDS,
                    "allowed_updates": ["message"],
                }),
            )
            .await?;
        let mut messages = Vec::new();
        for update in updates {
            // Acknowledge every update, handled or not, so none repeats.
            self.offset
                .fetch_max(update.update_id + 1, Ordering::AcqRel);
            if let Some(inbound) = update.message.and_then(Message::into_inbound) {
                messages.push(inbound);
            }
        }
        Ok(messages)
    }

    async fn send(&self, chat: i64, text: &str) -> Result<()> {
        for piece in chunks(text, MAX_MESSAGE_CHARS)? {
            let _sent: serde_json::Value = self
                .call("sendMessage", json!({"chat_id": chat, "text": piece}))
                .await?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Reply<R> {
    ok: bool,
    result: Option<R>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    from: Option<Account>,
    chat: Chat,
    text: Option<String>,
}

#[derive(Deserialize)]
struct Account {
    id: i64,
}

#[derive(Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}

impl Message {
    /// A text message from a person in a private chat; anything else is
    /// not handled.
    fn into_inbound(self) -> Option<Inbound> {
        if self.chat.kind != "private" {
            return None;
        }
        Some(Inbound {
            sender: self.from?.id,
            chat: self.chat.id,
            text: self.text.filter(|text| !text.trim().is_empty())?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn updates(source: serde_json::Value) -> Vec<Inbound> {
        serde_json::from_value::<Vec<Update>>(source)
            .unwrap()
            .into_iter()
            .filter_map(|update| update.message.and_then(Message::into_inbound))
            .collect()
    }

    #[test]
    fn only_text_from_a_person_in_a_private_chat_is_a_message() {
        let messages = updates(json!([
            {"update_id": 1, "message": {
                "message_id": 5, "from": {"id": 42, "is_bot": false},
                "chat": {"id": 42, "type": "private"}, "text": "hello"}},
            // A group chat, a photo with no text, and a channel post with no sender.
            {"update_id": 2, "message": {
                "from": {"id": 42}, "chat": {"id": -100, "type": "group"}, "text": "hi all"}},
            {"update_id": 3, "message": {
                "from": {"id": 42}, "chat": {"id": 42, "type": "private"}}},
            {"update_id": 4, "message": {
                "chat": {"id": 42, "type": "private"}, "text": "anonymous"}},
            {"update_id": 5},
        ]));
        assert_eq!(
            messages,
            [Inbound {
                sender: 42,
                chat: 42,
                text: "hello".into()
            }]
        );
    }

    #[test]
    fn a_missing_token_is_reported_by_variable_name_only() {
        let error = Telegram::from_env("XSHELL_GATEWAY_TEST_UNSET_TOKEN")
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("XSHELL_GATEWAY_TEST_UNSET_TOKEN")
        );
    }
}
