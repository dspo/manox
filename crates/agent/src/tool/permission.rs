//! Tool-call permissions.
//!
//! Session-scoped always-allow cache: once a user picks "always allow" for a
//! tool, it is not re-prompted within the session. Not persisted across sessions.

use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Allow for this call only.
    AllowOnce,
    /// Always allow this tool for the rest of the session.
    AlwaysAllow,
    /// Deny (an error is fed back to the model).
    Deny,
}

/// Session-scoped permission cache (thread-safe).
#[derive(Default)]
pub struct PermissionCache {
    always_allow: Mutex<HashSet<String>>,
}

impl PermissionCache {
    pub fn is_always_allowed(&self, tool_name: &str) -> bool {
        self.always_allow
            .lock()
            .expect("always_allow poisoned")
            .contains(tool_name)
    }

    pub fn set_always_allowed(&self, tool_name: &str) {
        self.always_allow
            .lock()
            .expect("always_allow poisoned")
            .insert(tool_name.to_string());
    }

    pub fn clear(&self) {
        self.always_allow
            .lock()
            .expect("always_allow poisoned")
            .clear();
    }
}
