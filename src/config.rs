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
    /// Web search for the `web` tool group; absent means no web tools.
    #[serde(default)]
    pub web_search: Option<WebSearch>,
    /// Periodic check-ins; absent means none.
    #[serde(default)]
    pub heartbeat: Option<Heartbeat>,
    #[serde(default, rename = "users")]
    pub users: Vec<User>,
}

/// Which search service `web_search` uses (see [`crate::web`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearch {
    /// `searxng` or `brave`.
    pub backend: String,
    /// The SearXNG instance's address.
    #[serde(default)]
    pub url: Option<String>,
    /// The name of the environment variable holding the Brave API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

/// The tool groups a user can be given, and the tools in each.
pub const TOOL_GROUPS: [(&str, &[&str]); 3] = [
    ("memory", &crate::memory::TOOL_NAMES),
    ("schedule", &crate::schedule::TOOL_NAMES),
    ("web", &crate::web::TOOL_NAMES),
];

/// xshell's own shell tool, given by a user's `shell` setting.
const SHELL_TOOL: &str = "run_shell";

/// Whether a user's agent may run shell commands, and how they are held in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shell {
    /// Any command, without asking, as the account `sybild` runs under.
    /// Nothing confines it to this user's data. What it does is still
    /// subject to the xshell configuration: its approval policy can deny
    /// commands, and its audit log records every one.
    Unrestricted,
}

/// When heartbeats run (see [`crate::heartbeat`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    /// Minutes between one user's heartbeats; at least 5.
    pub every_minutes: u32,
    /// Local hours heartbeats may run in, as `HH:MM-HH:MM`; any time when
    /// absent.
    #[serde(default)]
    pub active_hours: Option<String>,
}

impl Heartbeat {
    pub fn active_hours(&self) -> Result<Option<crate::heartbeat::ActiveHours>> {
        self.active_hours
            .as_deref()
            .map(crate::heartbeat::ActiveHours::parse)
            .transpose()
    }
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
    /// The tool groups this user's agent has: any of `memory`, `schedule`
    /// and `web`. Without it the user gets `memory` and `schedule`, plus
    /// `web` when `[web_search]` is configured.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Shell access; none when absent.
    #[serde(default)]
    pub shell: Option<Shell>,
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
        if let Some(heartbeat) = &self.heartbeat {
            if heartbeat.every_minutes < 5 {
                bail!("heartbeat.every_minutes must be at least 5");
            }
            heartbeat.active_hours()?;
        }
        if let Some(search) = &self.web_search {
            match search.backend.as_str() {
                "searxng" if search.url.is_none() => {
                    bail!("web_search.url is required for the searxng backend")
                }
                "brave" if search.api_key_env.is_none() => {
                    bail!("web_search.api_key_env is required for the brave backend")
                }
                "searxng" | "brave" => {}
                other => {
                    bail!("unknown web_search.backend {other:?}; use \"searxng\" or \"brave\"")
                }
            }
        }
        for user in &self.users {
            let mut seen = HashSet::new();
            for group in user.tools.iter().flatten() {
                if !TOOL_GROUPS.iter().any(|(name, _)| name == group) {
                    bail!(
                        "user {:?} has unknown tool group {group:?}; the groups are memory, \
schedule and web{}",
                        user.name,
                        if group == "shell" {
                            " (shell access is the separate `shell` setting)"
                        } else {
                            ""
                        }
                    );
                }
                if !seen.insert(group.as_str()) {
                    bail!("user {:?} lists tool group {group:?} twice", user.name);
                }
                if group == "web" && self.web_search.is_none() {
                    bail!(
                        "user {:?} has the web tool group, but there is no [web_search] section",
                        user.name
                    );
                }
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

    /// The tool groups `user`'s agent has, in a fixed order.
    pub fn tool_groups(&self, user: &str) -> Vec<&'static str> {
        let chosen = self.user(user).and_then(|user| user.tools.as_ref());
        TOOL_GROUPS
            .iter()
            .map(|(group, _)| *group)
            .filter(|group| match chosen {
                Some(chosen) => chosen.iter().any(|name| name == group),
                None => match *group {
                    "memory" | "schedule" => true,
                    "web" => self.web_search.is_some(),
                    _ => false,
                },
            })
            .collect()
    }

    /// The names of the tools `user`'s session is limited to.
    pub fn session_tools(&self, user: &str) -> Vec<String> {
        let groups = self.tool_groups(user);
        let mut tools: Vec<String> = TOOL_GROUPS
            .iter()
            .filter(|(group, _)| groups.contains(group))
            .flat_map(|(_, tools)| tools.iter().map(|tool| (*tool).to_owned()))
            .collect();
        if self.shell(user).is_some() {
            tools.push(SHELL_TOOL.to_owned());
        }
        tools
    }

    /// `user`'s shell access, if they have any.
    pub fn shell(&self, user: &str) -> Option<Shell> {
        self.user(user).and_then(|user| user.shell)
    }

    /// Check that the embedded daemon will let users with an unrestricted
    /// shell run commands unattended. Its approval ceiling is `ask` unless
    /// the xshell configuration raises it; under `ask` every command would
    /// wait for an approval nobody can give from chat and be refused, which
    /// looks like a shell that never works.
    pub fn check_shell_ceiling(&self) -> Result<()> {
        let Some(user) = self.users.iter().find(|user| user.shell.is_some()) else {
            return Ok(());
        };
        let ceiling = match &self.xshell_config {
            Some(path) => {
                let source = std::fs::read_to_string(path)
                    .with_context(|| format!("cannot read xshell_config {}", path.display()))?;
                let parsed: toml::Table = toml::from_str(&source)
                    .with_context(|| format!("invalid xshell_config {}", path.display()))?;
                parsed
                    .get("session_fabric")
                    .and_then(|fabric| fabric.get("max_approval"))
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            }
            None => None,
        };
        if ceiling.as_deref() != Some("auto") {
            bail!(
                "user {:?} has shell = \"unrestricted\", which needs the xshell configuration \
(xshell_config) to contain\n\n    [session_fabric]\n    max_approval = \"auto\"\n\n\
so that the session daemon runs their commands without asking",
                user.name
            );
        }
        Ok(())
    }

    pub fn user(&self, name: &str) -> Option<&User> {
        self.users.iter().find(|user| user.name == name)
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
        let heartbeat = config.heartbeat.as_ref().unwrap();
        assert_eq!(heartbeat.every_minutes, 30);
        assert!(heartbeat.active_hours().unwrap().is_some());
    }

    #[test]
    fn users_get_default_tool_groups_unless_they_list_their_own() {
        let config: Config = toml::from_str(EXAMPLE).unwrap();
        // The example configures web search, gives alice a shell, and leaves
        // bob on the defaults.
        assert_eq!(config.tool_groups("alice"), ["memory", "schedule", "web"]);
        assert_eq!(config.shell("alice"), Some(Shell::Unrestricted));
        assert_eq!(config.shell("bob"), None);
        assert!(
            !config
                .session_tools("bob")
                .contains(&"run_shell".to_owned())
        );
        assert_eq!(config.tool_groups("bob"), ["memory", "schedule", "web"]);
        assert_eq!(
            config.session_tools("bob"),
            [
                "memory_read",
                "memory_list",
                "memory_write",
                "schedule_add",
                "schedule_list",
                "schedule_remove",
                "web_search"
            ]
        );
        assert_eq!(config.session_tools("alice").last().unwrap(), "run_shell");

        // Without [web_search], the default leaves web out.
        with_users("[[users]]\nname = \"a\"\n").unwrap();
        let plain: Config = toml::from_str(
            "workspace = \"/w\"\nstate_directory = \"/s\"\n[model]\nprovider = \"p\"\n\
model = \"m\"\nbase_url = \"u\"\n[[users]]\nname = \"a\"\n[[users]]\nname = \"b\"\n\
tools = [\"memory\"]\n",
        )
        .unwrap();
        assert_eq!(plain.tool_groups("a"), ["memory", "schedule"]);
        // A listed selection replaces the default; it does not add to it.
        assert_eq!(
            plain.session_tools("b"),
            ["memory_read", "memory_list", "memory_write"]
        );
    }

    #[test]
    fn tool_groups_and_search_settings_are_checked() {
        for (extra, message) in [
            (
                "[[users]]\nname = \"a\"\ntools = [\"exec\"]\n",
                "unknown tool group",
            ),
            (
                "[[users]]\nname = \"a\"\ntools = [\"memory\", \"memory\"]\n",
                "twice",
            ),
            (
                "[[users]]\nname = \"a\"\ntools = [\"web\"]\n",
                "no [web_search] section",
            ),
            ("[web_search]\nbackend = \"searxng\"\n", "url is required"),
            (
                "[web_search]\nbackend = \"brave\"\n",
                "api_key_env is required",
            ),
            (
                "[web_search]\nbackend = \"bing\"\n",
                "unknown web_search.backend",
            ),
        ] {
            let error = with_users(extra).unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
        with_users(
            "[web_search]\nbackend = \"searxng\"\nurl = \"https://s.example\"\n\
[[users]]\nname = \"a\"\ntools = [\"web\"]\nshell = \"unrestricted\"\n",
        )
        .unwrap();
        // `shell` is not a tool group, and the error says where it goes.
        let error = with_users("[[users]]\nname = \"a\"\ntools = [\"shell\"]\n").unwrap_err();
        assert!(
            format!("{error:#}").contains("separate `shell` setting"),
            "{error:#}"
        );
        assert!(with_users("[[users]]\nname = \"a\"\nshell = \"sudo\"\n").is_err());
    }

    #[test]
    fn an_unrestricted_shell_needs_the_daemon_ceiling_raised() {
        let temporary = tempfile::TempDir::new().unwrap();
        let xshell = temporary.path().join("xshell.toml");
        let config = |shell: &str, xshell_config: Option<&std::path::Path>| -> Config {
            let extra = xshell_config
                .map(|path| format!("xshell_config = \"{}\"\n", path.display()))
                .unwrap_or_default();
            toml::from_str(&format!(
                "workspace = \"/w\"\nstate_directory = \"/s\"\n{extra}[model]\n\
provider = \"p\"\nmodel = \"m\"\nbase_url = \"u\"\n[[users]]\nname = \"rich\"\n{shell}"
            ))
            .unwrap()
        };
        // No shell users: nothing to check, with or without a file.
        config("", None).check_shell_ceiling().unwrap();
        let unrestricted = "shell = \"unrestricted\"\n";
        // No xshell configuration, or one that leaves the ceiling at ask.
        let error = config(unrestricted, None)
            .check_shell_ceiling()
            .unwrap_err();
        assert!(
            error.to_string().contains("max_approval = \"auto\""),
            "{error:#}"
        );
        for contents in ["", "[session_fabric]\nmax_approval = \"ask\"\n"] {
            std::fs::write(&xshell, contents).unwrap();
            assert!(
                config(unrestricted, Some(&xshell))
                    .check_shell_ceiling()
                    .is_err()
            );
        }
        std::fs::write(&xshell, "[session_fabric]\nmax_approval = \"auto\"\n").unwrap();
        config(unrestricted, Some(&xshell))
            .check_shell_ceiling()
            .unwrap();
    }

    #[test]
    fn heartbeat_settings_are_checked() {
        for (heartbeat, message) in [
            ("[heartbeat]\nevery_minutes = 1\n", "at least 5"),
            (
                "[heartbeat]\nevery_minutes = 30\nactive_hours = \"all day\"\n",
                "HH:MM-HH:MM",
            ),
        ] {
            let error = with_users(heartbeat).unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
        with_users("[heartbeat]\nevery_minutes = 30\n").unwrap();
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
