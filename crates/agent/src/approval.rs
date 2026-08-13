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
//! The host-private Approval agent uses a fixed English policy prompt and a
//! structured terminal tool; conversation language remains evidence rather
//! than a policy axis.

/// Verdict the reviewer returns for a single tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewVerdict {
    /// The tool is safe to run without prompting the user.
    Allow,
    /// The reviewer could not auto-approve; the tool is denied and `reason`
    /// is returned to the model so it can adjust its approach.
    Ask { reason: String },
}
