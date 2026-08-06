//! Built-in approval reviewer agent.
//!
//! When the thread's `ApprovalMode` is `AutoPilot`, each tool call that would
//! normally require approval is instead vetted by [`review`] before running.
//! The reviewer makes a single-shot LLM call (no tools, no streaming) and
//! returns one of two verdicts:
//!
//! - [`ReviewVerdict::Allow`] — the tool runs immediately.
//! - [`ReviewVerdict::Ask { reason }`] — the tool is denied and the `reason`
//!   is returned to the model so it can adjust its approach.
//!
//! Failures (LLM unavailable, timeout, malformed response) **all** downgrade
//! to `Ask` with a generic reason — the reviewer is fail-closed so a broken
//! autopilot path never silently widens access.
//!
//! The reviewer prompt lives in the `side_call/approval_system.tera.md`
//! template and is rendered at the request-build boundary. It is model-facing
//! text; it is bilingual via the thread's `agent_language` (en / zh-CN
//! mirrors) and is never routed through the `i18n` bundle (which only carries
//! UI chrome).

use std::collections::HashMap;

use crate::language_model::TokenUsage;

/// Per-call hard timeout for the reviewer. The reviewer is allowed to take
/// longer than a streaming chunk — the user is already waiting for the tool
/// to run, so a couple of seconds for an LLM judgment is acceptable. Past
/// this bound we fail-closed to `Ask`.

/// Verdict the reviewer returns for a single tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewVerdict {
    /// The tool is safe to run without prompting the user.
    Allow,
    /// The reviewer could not auto-approve; the tool is denied and `reason`
    /// is returned to the model so it can adjust its approach.
    Ask { reason: String },
}

#[derive(Debug, Clone)]
pub struct ReviewItem {
    pub id: String,
    pub tool_name: String,
    pub tool_title: String,
    pub tool_input: serde_json::Value,
}

pub struct ReviewBatchOutcome {
    pub verdicts: HashMap<String, ReviewVerdict>,
    pub usage: Option<TokenUsage>,
    pub model_name: String,
}

pub struct ReviewOutcome {
    pub verdict: ReviewVerdict,
    pub usage: Option<TokenUsage>,
    pub model_name: String,
}

