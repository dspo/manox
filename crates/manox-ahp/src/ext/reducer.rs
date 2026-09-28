//! The `x-manox` state fold.
//!
//! AHP's reducers ignore private actions by construction — every upstream
//! reducer's fallback arm is `OutOfScope` for `StateAction::Unknown` — so
//! manox-only state needs its own reducer, and this is it.
//!
//! Where it lives matters and is worth stating with the evidence: the Rust
//! `ahp_types::state::SnapshotState` enum has exactly nine arms
//! (`Session`/`Chat`/`Terminal`/`Changeset`/`ResourceWatch`/`Annotations`/
//! `Automations`/`AutomationRun`/`Root`) and **no generic arm**, so a private
//! channel cannot answer `subscribe` with a typed snapshot carrying its state.
//! The host therefore folds extension state here for its own bookkeeping and
//! delivers it to subscribers as **extension action envelopes** (a baseline
//! pushed right after `subscribe`, then deltas) — which is exactly what the
//! declaration in [`super`] promises a client.
//!
//! Unknown-tolerant by design: an action this build does not know (a newer
//! client's extension, or a name we retired) leaves the state untouched and is
//! reported as [`Outcome::Unrecognised`], never as an error.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Everything manox carries that AHP has no native slot for, per session.
///
/// One bag per extension channel: a channel for one session's plan mode, another
/// for its background work, and so on all fold into the same shape, so the host
/// needs one fold rather than one per channel kind.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XManoxState {
    /// Plan mode toggle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode: Option<bool>,
    /// The plan document (kernel snapshot shape, opaque here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Value>,
    /// Plan review lifecycle: `proposed` / `resolved` plus the reviewed file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_review: Option<Value>,
    /// Session goal (`null` clears).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<Value>,
    /// Active browser suites.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_suites: Option<Vec<String>>,
    /// Background-task registry snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_tasks: Option<Value>,
    /// The sub-agent tree and its progress ticks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<Value>,
    /// Compaction state (started / finished with its summary facts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<Value>,
    /// Thread pinned flag — AHP has no pin bit, only read/archived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    /// Label row attached to the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Session-info annotation row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_info: Option<Value>,
    /// The journal's active-chain leaf (fork cursor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf: Option<String>,
    /// The engine's currently visible tool set (kernel diagnostic state).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tools: Option<Vec<String>>,
    /// Manual thread/workspace ordering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Value>,
}

/// What one extension action did to the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The action was recognised and changed the state.
    Applied,
    /// Recognised, but it restated what the state already said.
    NoOp,
    /// Not an extension action this build knows; the state is untouched.
    Unrecognised,
}

/// Fold one extension action into `state`.
pub fn apply(state: &mut XManoxState, action: &Value) -> Outcome {
    let Some(tag) = action.get("type").and_then(Value::as_str) else {
        return Outcome::Unrecognised;
    };
    let before = state.clone();
    match tag {
        super::actions::PLAN_MODE_CHANGED => {
            state.plan_mode = action.get("enabled").and_then(Value::as_bool);
        }
        super::actions::PLAN_CHANGED => {
            state.plan = action.get("snapshot").cloned();
        }
        super::actions::PLAN_VERDICT_REQUESTED | super::actions::PLAN_VERDICT => {
            // Both edges land in the same field: the review's lifecycle is one
            // fact (a proposal, then its verdict), and a client renders the
            // difference from the payload.
            state.plan_review = Some(action.clone());
        }
        super::actions::WORK_GOAL_CHANGED => {
            state.goal = action.get("goal").cloned();
        }
        super::actions::WORK_BROWSER_SUITES => {
            state.browser_suites = action
                .get("suites")
                .and_then(Value::as_array)
                .map(|suites| {
                    suites
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                });
        }
        super::actions::WORK_BACKGROUND_TASKS => {
            state.background_tasks = action.get("snapshot").cloned();
        }
        super::actions::WORK_SUBAGENTS => {
            state.subagents = action
                .get("tree")
                .cloned()
                .or_else(|| action.get("snapshot").cloned());
        }
        super::actions::WORK_ACTIVE_TOOLS => {
            state.active_tools = action.get("tools").and_then(Value::as_array).map(|tools| {
                tools
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            });
        }
        super::actions::METRICS_CHANGED => {
            // Metrics are a read model with no single field of their own; the
            // payload is the aggregate, kept verbatim under its own channel.
            state.session_info = Some(action.clone());
        }
        super::actions::PINNED_CHANGED => {
            state.pinned = action.get("pinned").and_then(Value::as_bool);
        }
        super::actions::LABEL_CHANGED => {
            state.label = action
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        super::actions::SESSION_INFO_CHANGED => {
            state.session_info = action.get("data").cloned();
        }
        super::actions::LEAF_CHANGED => {
            state.leaf = action
                .get("targetId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        super::actions::ORDER_CHANGED => {
            state.order = action.get("order").cloned();
        }
        _ => return Outcome::Unrecognised,
    }
    if *state == before {
        Outcome::NoOp
    } else {
        Outcome::Applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn folds_the_declared_actions() {
        let mut state = XManoxState::default();
        assert_eq!(
            apply(
                &mut state,
                &json!({"type": "x-manox-plan/planModeChanged", "enabled": true})
            ),
            Outcome::Applied
        );
        assert_eq!(state.plan_mode, Some(true));
        assert_eq!(
            apply(
                &mut state,
                &json!({"type": "x-manox/pinnedChanged", "pinned": true})
            ),
            Outcome::Applied
        );
        assert_eq!(state.pinned, Some(true));
        assert_eq!(
            apply(
                &mut state,
                &json!({"type": "x-manox-work/browserSuitesChanged", "suites": ["chrome"]})
            ),
            Outcome::Applied
        );
        assert_eq!(
            state.browser_suites.as_deref(),
            Some(["chrome".to_string()].as_slice())
        );
    }

    #[test]
    fn restating_the_same_fact_is_a_noop_and_the_state_survives_round_trips() {
        let mut state = XManoxState::default();
        let action = json!({"type": "x-manox-plan/planModeChanged", "enabled": true});
        assert_eq!(apply(&mut state, &action), Outcome::Applied);
        assert_eq!(apply(&mut state, &action), Outcome::NoOp);
        let encoded = serde_json::to_value(&state).expect("serializes");
        let decoded: XManoxState = serde_json::from_value(encoded).expect("round-trips");
        assert_eq!(state, decoded);
    }

    #[test]
    fn unknown_actions_are_ignored_rather_than_failing() {
        let mut state = XManoxState::default();
        assert_eq!(
            apply(
                &mut state,
                &json!({"type": "x-manox-something/ofTheFuture", "x": 1})
            ),
            Outcome::Unrecognised
        );
        assert_eq!(
            apply(&mut state, &json!({"no": "type"})),
            Outcome::Unrecognised
        );
        assert_eq!(state, XManoxState::default());
    }
}
