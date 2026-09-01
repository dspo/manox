//! Tool-call permissions.
//!
//! These types are the shared currency between the harness backends and the
//! UI: the pi harness gates tools through them (see `pi_approval`), and the
//! workspace sends its `AskUserQuestion` answers back as
//! [`ToolAuthorizationResponse`].

/// A pending interaction parked on the user's answer (`AskUserQuestion`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Allow for this call only.
    AllowOnce,
    /// Deny (an error is fed back to the model).
    Deny,
}

/// Payload the UI sends back through the authorization oneshot. Either a
/// bare allow/deny decision, or — for `AskUserQuestion` — the answers
/// collected from the user, which the thread short-circuits into a
/// `ToolResult` without ever executing the tool.
#[derive(Debug)]
pub enum ToolAuthorizationResponse {
    Decision(PermissionDecision),
    AskUserQuestion {
        /// (question text, selected labels joined by ", " or free-form "Other" text).
        answers: Vec<(String, String)>,
        /// Free-form reply that dismisses the whole question card; when set,
        /// `answers` is ignored.
        response: Option<String>,
    },
}

/// Metadata of a pending interaction, kept so the workspace can re-surface
/// the card when switching back to a thread that parked on an answer.
#[derive(Debug, Clone)]
pub struct PendingAuthMeta {
    pub tool_name: String,
    pub summary: String,
    pub input: serde_json::Value,
}
