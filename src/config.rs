//! The gateway's configuration file.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use xshell_session::ModelBinding;

/// Everything the gateway reads from its configuration file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The assistant's workspace: `SOUL.md`, `IDENTITY.md`, `AGENTS.md` and
    /// the `memory/` tree (see [`crate::workspace::Workspace`]).
    pub workspace: PathBuf,
    /// Where the embedded session daemon keeps its state and socket.
    pub state_directory: PathBuf,
    /// The xshell configuration file for the embedded daemon: model
    /// profiles, limits, audit. Without it the daemon uses its defaults.
    #[serde(default)]
    pub xshell_config: Option<PathBuf>,
    /// The model every user's session is bound to.
    pub model: Model,
    #[serde(default)]
    pub telegram: Option<Telegram>,
    #[serde(default, rename = "users")]
    pub users: Vec<User>,
}

/// A model binding. When it names `api_key_env`, the embedded daemon accepts
/// it only if `xshell_config` has a `[models.NAME]` profile with the same
/// provider, base URL and variable.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    #[serde(default)]
    pub profile_name: Option<String>,
    pub provider: String,
    pub model: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub max_history_bytes: Option<usize>,
    #[serde(default)]
    pub context_tokens: Option<usize>,
    #[serde(default)]
    pub reserve_output_tokens: Option<usize>,
}

impl Model {
    pub fn binding(&self) -> ModelBinding {
        ModelBinding {
            profile_name: self.profile_name.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            api_key_env: self.api_key_env.clone(),
            max_history_bytes: self.max_history_bytes,
            context_tokens: self.context_tokens,
            reserve_output_tokens: self.reserve_output_tokens,
            // Images from chat are not handled yet.
            vision: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telegram {
    /// The name of the environment variable holding the bot token. The
    /// token itself never goes in this file.
    pub token_env: String,
}

/// One person the assistant talks to.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    /// Names the user's memory directory and session, so it is restricted
    /// to a-z, 0-9, `_` and `-`, starting with a letter.
    pub name: String,
    /// The user's Telegram account. Messages from any other account are
    /// refused.
    #[serde(default)]
    pub telegram_id: Option<i64>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read configuration {}", path.display()))?;
        let config: Self = toml::from_str(&source)
            .with_context(|| format!("invalid configuration {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for (name, path) in [
            ("workspace", &self.workspace),
            ("state_directory", &self.state_directory),
        ] {
            if !path.is_absolute() {
                bail!("{name} must be an absolute path, not {}", path.display());
            }
        }
        let mut names = HashSet::new();
        let mut accounts = HashSet::new();
        for user in &self.users {
            validate_user_name(&user.name)?;
            if !names.insert(user.name.as_str()) {
                bail!("user {:?} is listed twice", user.name);
            }
            if let Some(id) = user.telegram_id
                && !accounts.insert(id)
            {
                bail!("Telegram account {id} belongs to more than one user");
            }
        }
        Ok(())
    }

    /// The embedded daemon's socket.
    pub fn socket(&self) -> PathBuf {
        self.state_directory.join("xshelld.sock")
    }

    pub fn user_for_telegram(&self, account: i64) -> Option<&User> {
        self.users
            .iter()
            .find(|user| user.telegram_id == Some(account))
    }
}

/// A user name becomes a directory name and part of a session name.
pub fn validate_user_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = name.len() <= 32
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !valid {
        bail!(
            "user name {name:?} must be 1 to 32 characters of a-z, 0-9, _ and -, starting with \
a letter"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../config.example.toml");

    #[test]
    fn the_example_configuration_is_valid() {
        let config: Config = toml::from_str(EXAMPLE).unwrap();
        config.validate().unwrap();
        assert_eq!(config.users.len(), 2);
        assert_eq!(config.user_for_telegram(111111111).unwrap().name, "alice");
        assert!(config.user_for_telegram(1).is_none());
        assert_eq!(config.socket(), config.state_directory.join("xshelld.sock"));
        assert!(!config.model.binding().vision);
    }

    fn with_users(users: &str) -> Result<()> {
        let source = format!(
            "workspace = \"/w\"\nstate_directory = \"/s\"\n\
[model]\nprovider = \"ollama\"\nmodel = \"m\"\nbase_url = \"http://localhost:11434\"\n{users}"
        );
        toml::from_str::<Config>(&source)?.validate()
    }

    #[test]
    fn user_names_and_accounts_must_be_safe_and_unique() {
        with_users("[[users]]\nname = \"alice\"\ntelegram_id = 1\n").unwrap();
        for (users, message) in [
            ("[[users]]\nname = \"../bob\"\n", "must be 1 to 32"),
            ("[[users]]\nname = \"Alice\"\n", "must be 1 to 32"),
            ("[[users]]\nname = \"\"\n", "must be 1 to 32"),
            (
                "[[users]]\nname = \"a\"\n[[users]]\nname = \"a\"\n",
                "listed twice",
            ),
            (
                "[[users]]\nname = \"a\"\ntelegram_id = 7\n[[users]]\nname = \"b\"\ntelegram_id = 7\n",
                "more than one user",
            ),
        ] {
            let error = with_users(users).unwrap_err();
            assert!(format!("{error:#}").contains(message), "{users}: {error:#}");
        }
    }

    #[test]
    fn paths_must_be_absolute_and_unknown_keys_are_refused() {
        let relative = "workspace = \"w\"\nstate_directory = \"/s\"\n\
[model]\nprovider = \"p\"\nmodel = \"m\"\nbase_url = \"u\"\n";
        let error = toml::from_str::<Config>(relative)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(error.to_string().contains("absolute"));
        assert!(toml::from_str::<Config>(&format!("{relative}tokn = 1\n")).is_err());
    }
}
