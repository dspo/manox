//! The subagent seam's consumer-facing contracts: request, capability, and
//! result types plus the `start`/`end` observation payloads. A request that
//! needs a capability the chosen provider lacks is rejected with a typed
//! error rather than accepted-then-ignored (fail loud, no silent
//! degradation).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::core::env::ExecutionEnv;
use crate::core::ext_point_agent::AgentPermissions;
use crate::core::types::Model;

/// The smallest wall-clock or idle budget a delegation may arm — below it a
/// subagent cannot even land one model turn, so the budget is an input error
/// rather than an enforceable limit.
pub const MIN_BUDGET_MS: u64 = 1_000;

/// The largest wall-clock or idle budget a delegation may arm (30 days) —
/// past it the budget is indistinguishable from unbounded while still
/// holding an active-run slot, so a wedged delegation would keep its slot
/// for years. The settlement net's own cap is derived from this bound, so a
/// capped net can never fire before a budget this validator admits.
pub const MAX_BUDGET_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

/// Which START-TIME features a provider supports. Checked by the runtime
/// before delegating to [`crate::ext::subagent::SubagentProvider::start`].
/// Each flag corresponds one-to-one to a [`StartRequest`] option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Provider/model/reasoning-effort overrides on the child.
    pub agent_options: bool,
    /// `output_schema` on the child (structured capture). No in-process
    /// backend implements this yet; a request carrying a schema is rejected.
    pub output_schema: bool,
    /// A delegation-depth cap on the child.
    pub depth_limit: bool,
    /// Child tool scoping (allow/deny lists over the parent snapshot).
    pub tool_filter: bool,
    /// A declared (or dispatch-narrowed) permission ceiling on the child.
    pub permissions: bool,
    /// A per-request persona prefix shadowing the configured one.
    pub persona: bool,
    /// A turn cap on the child's drive loop (`max_turns`).
    pub max_turns: bool,
}

/// Why a start request was rejected before any provider work began.
#[derive(Debug, thiserror::Error)]
pub enum SubagentError {
    /// The named provider is not registered.
    #[error("unknown subagent provider `{0}`")]
    UnknownProvider(String),
    /// The request needs a capability the provider does not declare.
    #[error("provider `{provider}` does not support the requested capability `{capability}`")]
    UnsupportedCapability {
        provider: String,
        capability: &'static str,
    },
    /// The request is malformed (budget below minimum, depth overflow, …).
    #[error("{0}")]
    InvalidRequest(String),
    /// The provider failed while establishing the run.
    #[error("subagent provider `{0}` failed: {1}")]
    Provider(String, String),
}

/// Provider/model/reasoning-effort overrides and the thinking-replay policy
/// for the child, resolved by the caller against its provider registry before
/// the request is built (the seam carries detached data; resolution is the
/// caller's job).
#[derive(Debug, Clone, Default)]
pub struct AgentOptions {
    /// Explicit model for the child. `None` lets the provider apply its own
    /// precedence (inherited live model, assembly default).
    pub model: Option<Model>,
    /// Reasoning effort applied as the child's local thinking level.
    pub reasoning_effort: Option<String>,
    /// Replay thinking blocks from turns before the current one on later
    /// requests. Off by default: historical reasoning is dropped to save
    /// tokens, while the current turn's thinking is always preserved for the
    /// tool loop. Setting this restores verbatim replay.
    pub replay_thinking: bool,
}

/// Child tool scoping over the parent's snapshot. An empty `allow` means the
/// full snapshot (minus the strips every child gets); `deny` removes names
/// from whatever survives the allow pass. Unknown names fail loudly at
/// assembly instead of silently vanishing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ToolFilter {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

impl ToolFilter {
    /// Whether `name` survives the filter (allow-empty = allow-all).
    pub fn admits(&self, name: &str) -> bool {
        (self.allow.is_empty() || self.allow.iter().any(|n| n == name))
            && !self.deny.iter().any(|n| n == name)
    }
}

/// The armed time budgets of one delegation. `None` per axis leaves the run
/// unbounded there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Budgets {
    /// Wall-clock budget (ms): the run is terminated at this elapsed time.
    pub timeout_ms: Option<u64>,
    /// Idle budget (ms): the longest stretch with no child event and no tool
    /// in flight before the run is terminated as stalled.
    pub idle_timeout_ms: Option<u64>,
}

impl Budgets {
    /// Enforce the armed range [`MIN_BUDGET_MS`]..=[`MAX_BUDGET_MS`];
    /// `Ok(())` when every armed axis is enforceable.
    pub fn validate(&self) -> Result<(), SubagentError> {
        for (name, budget) in [
            ("timeout", self.timeout_ms),
            ("idle_timeout", self.idle_timeout_ms),
        ] {
            if budget.is_some_and(|t| t < MIN_BUDGET_MS) {
                return Err(SubagentError::InvalidRequest(format!(
                    "{name} must be at least {MIN_BUDGET_MS}ms"
                )));
            }
            if budget.is_some_and(|t| t > MAX_BUDGET_MS) {
                return Err(SubagentError::InvalidRequest(format!(
                    "{name} must be at most {MAX_BUDGET_MS}ms (30 days)"
                )));
            }
        }
        Ok(())
    }
}

/// What a caller asks for when starting a ONE-SHOT subagent. The tool layer
/// builds this from the model's `{ description, prompt }` plus its own
/// config; the runtime validates [`Capabilities`] against the named
/// provider and resolves the durable descriptor before dispatching to
/// [`crate::ext::subagent::SubagentProvider::start`].
pub struct StartRequest {
    /// Short display label persisted with the child (the tool call's
    /// `description`), used by surfaces and the background card.
    pub label: String,
    /// The definition kind the delegation tool was configured from
    /// (`Explore`, `Sailor`, a user manifest name) — the descriptor's
    /// identity key.
    pub kind: String,
    /// Task delivered as the child's user message.
    pub prompt: String,
    /// Host provider/model/effort overrides. Requires the `agent_options`
    /// capability.
    pub agent_options: Option<AgentOptions>,
    /// Absolute delegation-depth cap for the child: its computed depth must
    /// be ≤ this. Requires the `depth_limit` capability.
    pub max_depth: Option<u32>,
    /// Turn cap for the child's drive loop: the child ends with whatever it
    /// accumulated once the cap is reached (partial output, resumable).
    /// `None` = unbounded.
    pub max_turns: Option<u32>,
    /// Child tool scoping. Requires the `tool_filter` capability.
    pub tool_filter: Option<ToolFilter>,
    /// The child's permission ceiling, already intersected with any
    /// dispatch-time narrowing by the caller. `None` = inherit the
    /// dispatcher's authority live on both axes. Requires the `permissions`
    /// capability.
    pub permissions: Option<AgentPermissions>,
    /// Per-request persona prefix shadowing the configured one. Requires the
    /// `persona` capability.
    pub persona: Option<String>,
    /// The delegating parent's own depth (`0` = top-level thread). The child
    /// runs at `parent_depth + 1`.
    pub parent_depth: u32,
    /// Lineage: the parent's session id, written into the child header.
    pub parent_session: Option<String>,
    /// The parent-side tool call id that dispatched this run — the join key
    /// the `run/started` row carries so a card, a row, and a settled report
    /// correlate without guessing.
    pub tool_call_id: Option<String>,
    /// `Some(run_id)` marks a CONTINUATION: the settled child session named
    /// by the run id reopens and `prompt` is the follow-up. The run id IS
    /// the child session id, so the continuation keeps every pointer
    /// (`run/started`'s childSession, cards, drill-downs) intact.
    pub continuation_of: Option<String>,
    /// Arming wall-clock/idle budgets.
    pub budgets: Budgets,
    /// `Some("worktree")` isolates the child in a throwaway git worktree
    /// (manox original).
    pub isolation: Option<String>,
    /// The parent's execution environment (worktree git commands; the child
    /// session's default env comes from the provider assembly).
    pub env: Arc<dyn ExecutionEnv>,
    /// The parent's working directory — the child's cwd, and the repo a
    /// `worktree` isolation forks from.
    pub cwd: PathBuf,
    /// Cancellation from the spawning context — the canonical cancel channel
    /// both before and after startup.
    pub cancel: CancellationToken,
}

/// Provider-facing one-shot request after the runtime resolves the durable
/// child descriptor.
pub struct ResolvedStartRequest {
    /// The runtime-minted run id the provider must publish its handle
    /// under (the run table and observation events key off it).
    pub run_id: String,
    pub inner: StartRequest,
    pub descriptor: crate::ext::subagent::descriptor::Descriptor,
}

/// Why a subagent run ended. The vocabulary is merge-extensible: backends
/// may add variants; consumers branch on the known cases and treat unknown
/// as an error-class reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The child finished its turn normally.
    Completed,
    /// Cancelled through the request's cancel token.
    Aborted,
    /// Model, transport, budget, or stall failure.
    Error,
    /// The child hit its token ceiling before finishing.
    MaxTokens,
    /// The wall-clock budget expired and the run was stopped mid-flight.
    TimedOut,
    /// The idle budget expired with no child activity.
    Stalled,
    /// The foreground wait ended by promotion: the run continues in the
    /// background and settles later through its own continued run. This
    /// reason appears only on the promotion receipt, never on a settlement.
    Promoted,
    /// Reserved: the child declined the task.
    Refusal,
    /// A backend-added variant this build does not know.
    Unknown(String),
}

/// Symmetric with the merge-extensible deserialization: known variants
/// serialize to their kebab-case wire forms; an unknown round-trips as its
/// raw string.
impl Serialize for StopReason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl StopReason {
    /// Whether the run's `output` may be partial (anything non-completed).
    pub fn is_terminal_failure(&self) -> bool {
        matches!(
            self,
            StopReason::Error | StopReason::Unknown(_) | StopReason::TimedOut | StopReason::Stalled
        )
    }
}

/// Merge-extensible deserialization: known wire forms map to their variants;
/// anything a backend added beyond this build's vocabulary lands as
/// `Unknown` (consumers treat it as error-class) instead of failing.
impl<'de> serde::Deserialize<'de> for StopReason {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "completed" => StopReason::Completed,
            "aborted" => StopReason::Aborted,
            "error" => StopReason::Error,
            "max-tokens" => StopReason::MaxTokens,
            "refusal" => StopReason::Refusal,
            "timed-out" => StopReason::TimedOut,
            "stalled" => StopReason::Stalled,
            "promoted" => StopReason::Promoted,
            other => StopReason::Unknown(other.to_string()),
        })
    }
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::Completed => write!(f, "completed"),
            StopReason::Aborted => write!(f, "aborted"),
            StopReason::Error => write!(f, "error"),
            StopReason::MaxTokens => write!(f, "max-tokens"),
            StopReason::Refusal => write!(f, "refusal"),
            StopReason::TimedOut => write!(f, "timed-out"),
            StopReason::Stalled => write!(f, "stalled"),
            StopReason::Promoted => write!(f, "promoted"),
            StopReason::Unknown(s) => write!(f, "{s}"),
        }
    }
}

/// The terminal outcome of a subagent run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    /// The child's final assistant output: the text of its last non-empty
    /// assistant message, capped for the parent's context window. Empty when
    /// the child produced none.
    pub output: String,
    /// Structured result after a requested `output_schema` was satisfied.
    /// Always `None` while no backend implements the capability.
    pub structured: Option<serde_json::Value>,
    /// Provider-authored failure detail for a non-`completed` result (kept
    /// free of tool inputs, file contents, credentials; capped at 4096
    /// UTF-8 bytes).
    pub diagnostic: Option<String>,
    /// Why the run ended. A non-`completed` reason means `output` may be
    /// partial.
    pub stop_reason: StopReason,
}

/// Observe-only identifying detail for a published subagent run, carried by
/// the runtime's `start` event.
#[derive(Debug, Clone)]
pub struct RunInfo {
    /// Unique identity shared with the paired terminal event.
    pub run_id: String,
    /// Provider name the run was started on.
    pub provider: String,
    /// The dispatch label (the tool call's `description`).
    pub label: String,
    /// The definition kind the delegation tool was configured from.
    pub kind: String,
    /// The child's resolved model, for surfaces that name what runs.
    pub model: Option<Model>,
    /// Whether the child is an in-process session (always true today).
    pub local: bool,
    /// The dispatch's armed budgets — the host watchdog reads the idle
    /// budget from here.
    pub budgets: Budgets,
    /// The parent-side tool call id that dispatched this run.
    pub tool_call_id: Option<String>,
}

/// The static per-definition dispatch configuration, sourced from manox
/// agent manifests + the providers config. Definition resolution is
/// engine-internal: the unified `Steer` tool dispatches against this
/// config by registry name; no per-definition model-facing tool exists.
#[derive(Debug, Clone)]
pub struct SubagentDefConfig {
    /// The provider registry name dispatches run on (e.g. `spawn`).
    pub provider: String,
    /// The definition's registry name — the `Steer` `to.spawn` value and
    /// the run's `kind` identity.
    pub name: String,
    /// Capability tag (`read-only` / `write+bash` / …) surfaced in the
    /// `ListAgents` catalog.
    pub capability: String,
    /// The definition's own description (what it is for), as the catalog
    /// lists it.
    pub description: String,
    /// The definition body — the child's persona/system prompt.
    pub persona: String,
    /// The definition's `tools:` allow list; empty = the full snapshot.
    pub default_tools: Vec<String>,
    /// The definition's `disallowedTools:` deny list, removed from whatever
    /// survives the allow pass. Pre-resolved at assembly (unknown names
    /// warn-and-dropped there).
    pub default_denied_tools: Vec<String>,
    /// Raw `model:` frontmatter spec, resolved against the provider
    /// registry at dispatch.
    pub frontmatter_model_spec: Option<String>,
    /// Raw dedicated spec from the providers config `subagents:` map —
    /// wins over the frontmatter.
    pub config_model_spec: Option<String>,
    /// The definition's `effort:` mapped onto this runtime's vocabulary
    /// (`xhigh` → `max`) at assembly; `None` when undeclared or out of
    /// vocabulary. The lowest effort tier — a dispatch parameter or the
    /// config spec wins over it.
    pub frontmatter_effort: Option<String>,
    /// The definition's `isolation:` default, validated at assembly (only
    /// `worktree` survives); `None` = dispatch in place. A per-call
    /// `isolation` parameter wins over it.
    pub frontmatter_isolation: Option<String>,
    /// The definition's `background: true` default: dispatch onto a
    /// background card without the model asking.
    pub frontmatter_background: bool,
    /// The definition's `maxTurns:` cap on the child's drive loop.
    pub frontmatter_max_turns: Option<u32>,
    /// The definition's `color:` display data; rides the run rows to host
    /// surfaces. No behavior.
    pub color: Option<String>,
    /// The definition's declared permission ceiling (the `permissions:`
    /// frontmatter). A dispatch-time `permissions` parameter may narrow it,
    /// never widen it; `inherit` axes (the default) follow the dispatcher's
    /// own authority live.
    pub declared_permissions: AgentPermissions,
    /// Absolute delegation-depth cap for dispatches through this config.
    pub max_depth: u32,
}

/// Observe-only outcome detail for a settled run, paired with its
/// [`RunInfo`] by `run_id`.
#[derive(Debug, Clone)]
pub struct RunEndInfo {
    pub run_id: String,
    pub provider: String,
    pub label: String,
    pub local: bool,
    pub stop_reason: StopReason,
    /// The child's final output (same rule as [`RunResult::output`]).
    pub output: String,
}

/// The runtime's observation event pair (`start`/`end`).
#[derive(Debug, Clone)]
pub enum Event {
    Start(RunInfo),
    End(RunEndInfo),
}

/// Reject a request whose options exceed the provider's declared
/// capabilities. Fail-loud by design: an accepted-then-ignored option would
/// silently run the child under a different configuration than asked.
pub fn assert_capabilities(
    provider: &str,
    caps: &Capabilities,
    request: &StartRequest,
) -> Result<(), SubagentError> {
    let need = |supported: bool, capability: &'static str| -> Result<(), SubagentError> {
        if supported {
            Ok(())
        } else {
            Err(SubagentError::UnsupportedCapability {
                provider: provider.to_string(),
                capability,
            })
        }
    };
    if request.agent_options.is_some() {
        need(caps.agent_options, "agent_options")?;
    }
    if request.max_depth.is_some() {
        need(caps.depth_limit, "depth_limit")?;
    }
    if request.tool_filter.is_some() {
        need(caps.tool_filter, "tool_filter")?;
    }
    if request.permissions.is_some() {
        need(caps.permissions, "permissions")?;
    }
    if request.persona.is_some() {
        need(caps.persona, "persona")?;
    }
    if request.max_turns.is_some() {
        need(caps.max_turns, "max_turns")?;
    }
    Ok(())
}

/// Names no child ever receives: the parent's inter-agent messaging tool and
/// every tool whose run is a round trip to a human (the D5 guard — a child
/// session is never given a tool that interrupts a person, whether the
/// snapshot carries it or a definition names it explicitly).
pub const HUMAN_INTERACTION_TOOLS: &[&str] = &["AskUserQuestion"];

/// The model-facing reason a gated tool is rejected inside a subagent. Names
/// the delegation boundary and, unlike a silent refusal, tells the child
/// what it CAN still do so it reframes instead of retrying the denied call.
pub const SUBAGENT_APPROVAL_DENIED_REASON: &str = "This operation requires user approval, but a \
    subagent runs without an approval channel: operations that require approval are rejected \
    automatically. Read-only work (Read/Grep/Glob/Ls) and workspace-confined writes still run. \
    Stay within those bounds, or surface the approval-requiring step in your final summary so \
    the parent agent can perform it interactively.";

/// Convenience constructor used by tests and hosts assembling a request.
pub fn test_request(label: &str, prompt: &str) -> StartRequest {
    let cwd = std::env::temp_dir();
    StartRequest {
        label: label.to_string(),
        kind: label.to_string(),
        prompt: prompt.to_string(),
        agent_options: None,
        max_depth: None,
        max_turns: None,
        tool_filter: None,
        permissions: None,
        persona: None,
        parent_depth: 0,
        parent_session: None,
        tool_call_id: None,
        continuation_of: None,
        budgets: Budgets::default(),
        isolation: None,
        env: Arc::new(crate::core::env::TokioExecutionEnv::new(cwd.clone())),
        cwd,
        cancel: CancellationToken::new(),
    }
}

/// Snapshot of one live run served to the `list_agents` control tool.
#[derive(Debug, Clone, Serialize)]
pub struct RunSnapshot {
    pub id: String,
    /// The definition kind the delegation tool was configured from.
    pub kind: String,
    pub label: String,
    pub provider: String,
    /// Status vocabulary; one-shot runs are `running` until they settle.
    pub status: &'static str,
    pub running_for_ms: u64,
    pub local: bool,
}

/// Check a tool filter's named tools against the snapshot it will apply to;
/// unknown allow/deny names fail loudly so a typo cannot silently widen a
/// child's tools. Returns the set difference for the error message.
pub fn validate_filter_names(
    filter: &ToolFilter,
    snapshot: &HashSet<&str>,
) -> Result<(), SubagentError> {
    for name in filter.allow.iter().chain(filter.deny.iter()) {
        if !snapshot.contains(name.as_str()) {
            return Err(SubagentError::InvalidRequest(format!(
                "tool filter names `{name}` which is not in the caller's tool snapshot"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stop-reason vocabulary is merge-extensible: known variants
    /// serialize to their kebab-case wire forms, and a backend-added variant
    /// deserializes as `Unknown` (never a deserialize error).
    #[test]
    fn stop_reason_serializes_to_kebab_wire_forms() {
        assert_eq!(
            serde_json::to_value(StopReason::Completed).unwrap(),
            serde_json::json!("completed")
        );
        assert_eq!(
            serde_json::to_value(StopReason::MaxTokens).unwrap(),
            serde_json::json!("max-tokens")
        );
        assert_eq!(
            serde_json::to_value(StopReason::Unknown("team-policy".into())).unwrap(),
            serde_json::json!("team-policy")
        );
    }

    #[test]
    fn stop_reason_deserializes_unknown_variants() {
        let reason: StopReason = serde_json::from_value(serde_json::json!("refusal")).unwrap();
        assert_eq!(reason, StopReason::Refusal);
        let reason: StopReason = serde_json::from_value(serde_json::json!("org-specific")).unwrap();
        assert_eq!(reason, StopReason::Unknown("org-specific".into()));
    }

    /// Capability negotiation is fail-loud: every requested option must have
    /// a matching declared capability bit.
    #[test]
    fn assert_capabilities_rejects_each_unsupported_option() {
        let caps = Capabilities {
            agent_options: false,
            output_schema: false,
            depth_limit: false,
            tool_filter: false,
            permissions: false,
            persona: false,
            max_turns: false,
        };
        let mut request = test_request("l", "p");
        request.persona = Some("p".into());
        let err = assert_capabilities("p", &caps, &request).unwrap_err();
        assert!(err.to_string().contains("persona"), "{err}");

        request.persona = None;
        let mut filter = ToolFilter::default();
        filter.allow.push("Read".into());
        request.tool_filter = Some(filter);
        let err = assert_capabilities("p", &caps, &request).unwrap_err();
        assert!(err.to_string().contains("tool_filter"), "{err}");

        request.tool_filter = None;
        request.permissions = Some(AgentPermissions::default());
        let err = assert_capabilities("p", &caps, &request).unwrap_err();
        assert!(err.to_string().contains("permissions"), "{err}");

        request.permissions = None;
        request.max_turns = Some(10);
        let err = assert_capabilities("p", &caps, &request).unwrap_err();
        assert!(err.to_string().contains("max_turns"), "{err}");
    }

    /// Filter validation: unknown allow/deny names fail loudly so a typo
    /// cannot silently change a child's tool surface.
    #[test]
    fn validate_filter_names_rejects_unknown_names() {
        let snapshot: HashSet<&str> = ["Read", "Bash"].into_iter().collect();
        let filter = ToolFilter {
            allow: vec!["Read".into()],
            deny: vec!["Bash".into()],
        };
        assert!(validate_filter_names(&filter, &snapshot).is_ok());
        let filter = ToolFilter {
            allow: vec!["Read".into(), "Raed".into()],
            deny: vec![],
        };
        let err = validate_filter_names(&filter, &snapshot).unwrap_err();
        assert!(err.to_string().contains("`Raed`"), "{err}");
    }

    /// Budgets are armed inside a closed range: below it the child cannot land
    /// one turn, above it the run is effectively unbounded while still holding
    /// its active-run slot — and the settlement net's cap is derived from the
    /// upper bound, so admitting a larger budget would let the cap fire before
    /// the caller's own deadline.
    #[test]
    fn budgets_enforce_the_armed_range() {
        let at_min = Budgets {
            timeout_ms: Some(MIN_BUDGET_MS),
            idle_timeout_ms: Some(MIN_BUDGET_MS),
        };
        assert!(at_min.validate().is_ok());
        let at_max = Budgets {
            timeout_ms: Some(MAX_BUDGET_MS),
            idle_timeout_ms: Some(MAX_BUDGET_MS),
        };
        assert!(at_max.validate().is_ok());
        assert!(Budgets::default().validate().is_ok(), "no budget is valid");

        let below = Budgets {
            timeout_ms: Some(MIN_BUDGET_MS - 1),
            idle_timeout_ms: None,
        };
        let err = below.validate().unwrap_err();
        assert!(err.to_string().contains("at least"), "{err}");

        let above = Budgets {
            timeout_ms: None,
            idle_timeout_ms: Some(MAX_BUDGET_MS + 1),
        };
        let err = above.validate().unwrap_err();
        assert!(err.to_string().contains("at most"), "{err}");
    }
}
