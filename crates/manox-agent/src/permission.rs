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

/// One canonical answer to one `AskUserQuestion` question, routed by the
/// question's stable `id` (never its text — the dsh L1 vocabulary).
///
/// Tri-state, encoded entirely in the `(selected, custom)` pair:
/// - **option selection**: `selected` non-empty, `custom: None` — the chosen
///   option labels.
/// - **custom text**: `selected` empty, `custom: Some(text)` — typed free
///   text that REPLACES the selection (single-select); with `selected`
///   non-empty it SUPPLEMENTS the selection (multi-select).
/// - **explicit skip**: `selected` empty, `custom: None` (an all-whitespace
///   `custom` normalises to `None`) — the user saw the question and declined
///   to answer it. Distinct from a card dismissal
///   ([`ToolAuthorizationResponse::AskUserQuestionDismissed`]) and from a
///   lapsed delivery ([`ToolAuthorizationResponse::AskUserQuestionExpired`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskAnswer {
    /// The question's stable id, matching the `id` the server minted onto the
    /// parked request. Unknown ids are dropped at the settle boundary.
    pub id: String,
    /// Selected option labels (empty when nothing was picked).
    pub selected: Vec<String>,
    /// Free-form custom text; `None` when absent or blank.
    pub custom: Option<String>,
}

impl AskAnswer {
    /// Normalising constructor: trims each selected label, drops empties, and
    /// collapses a blank `custom` to `None` so an explicit skip is exactly
    /// `{ selected: [], custom: None }`.
    pub fn new(id: String, selected: Vec<String>, custom: Option<String>) -> Self {
        Self {
            id,
            selected: selected
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            custom: custom
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty()),
        }
    }

    /// The explicit-skip state: nothing selected, no custom text.
    pub fn is_skip(&self) -> bool {
        self.selected.is_empty() && self.custom.is_none()
    }
}

/// Payload the UI sends back through the authorization oneshot. Either a
/// bare allow/deny decision, or — for `AskUserQuestion` — the answers
/// collected from the user, which the thread short-circuits into a
/// `ToolResult` without ever executing the tool.
#[derive(Debug)]
pub enum ToolAuthorizationResponse {
    Decision(PermissionDecision),
    AskUserQuestion {
        /// The canonical id-routed tri-state answers, one entry per answered
        /// (or explicitly skipped) question. There is no card-level free-text
        /// override: a card close is `AskUserQuestionDismissed`, and any
        /// per-question free text rides the answer's `custom`.
        answers: Vec<AskAnswer>,
    },
    /// The question settled without any user input (adjudication timeout,
    /// withdrawn delivery, or no capable client). Distinct from an empty
    /// `AskUserQuestion` so the model never reads a non-answer as an answer.
    AskUserQuestionExpired,
    /// The user CLOSED the question card to speak instead — an explicit
    /// "not now, let me talk" that is neither an answer, a rejection, nor a
    /// turn interrupt. The tool result tells the model to stop and wait for
    /// the forthcoming message (dsh `ASK_CANCELLED`), so the model neither
    /// treats silence as consent nor reads the close as a denial. Distinct
    /// from `AskUserQuestionExpired` (a delivery that lapsed with no human
    /// action) so the two causes surface different model guidance.
    AskUserQuestionDismissed,
}

/// Metadata of a pending interaction, kept so the workspace can re-surface
/// the card when switching back to a thread that parked on an answer.
#[derive(Debug, Clone)]
pub struct PendingAuthMeta {
    pub tool_name: String,
    pub summary: String,
    pub input: serde_json::Value,
}
