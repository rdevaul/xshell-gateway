//! Which user each daemon session belongs to.
//!
//! The gateway creates one session per user and records it here. The memory
//! tools and the context provider are told only a session id by the daemon,
//! and look the user up here. A session that is not in the directory, such
//! as one a person opened with the `xshell` client, belongs to no user: it
//! gets no personal memory and the memory tools refuse it.

use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Debug, Default)]
pub struct Directory {
    users: RwLock<HashMap<String, String>>,
}

impl Directory {
    /// Record that `session_id` is `user`'s session.
    pub fn register(&self, session_id: &str, user: &str) {
        self.write().insert(session_id.to_owned(), user.to_owned());
    }

    /// The user `session_id` belongs to, if the gateway created it.
    pub fn user_for(&self, session_id: &str) -> Option<String> {
        self.users
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, String>> {
        self.users
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The name of a user's session on the daemon.
pub fn session_name(user: &str) -> String {
    format!("chat-{user}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_registered_sessions_have_a_user() {
        let directory = Directory::default();
        directory.register("s1", "alice");
        assert_eq!(directory.user_for("s1").as_deref(), Some("alice"));
        assert_eq!(directory.user_for("s2"), None);
        // A session re-created for the same user replaces nothing else.
        directory.register("s3", "alice");
        assert_eq!(directory.user_for("s1").as_deref(), Some("alice"));
        assert_eq!(session_name("alice"), "chat-alice");
    }
}
