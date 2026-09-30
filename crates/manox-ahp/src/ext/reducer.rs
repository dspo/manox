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
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

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
    /// Background-task registry view: task id → the task's latest wire
    /// snapshot. Journal rows carry one task's snapshot each, so the fold
    /// upserts by `task_id` and has no clear edge — `None` means no task row
    /// has been folded, and terminal tasks keep their row (bounded by the
    /// session's distinct task count).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_tasks: Option<BTreeMap<String, Value>>,
    /// Sub-agent registry view: agent id → the agent's latest progress row
    /// (envelope tag stripped). Progress rows carry one agent's tick each
    /// (nesting is structurally off this iteration), so the fold upserts by
    /// `agentId`; `None` means no row has been folded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<BTreeMap<String, Value>>,
    /// Thread pinned flag — AHP has no pin bit, only read/archived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    /// Label row attached to the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Session-info annotation row. The bag is per channel, so on
    /// `x-manox-thread:/` this holds the session-info row while on
    /// `x-manox-metrics:/` it holds the metrics aggregate (the metrics
    /// read model has no field of its own).
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
    // The generic NoOp detector clones the whole bag; the two keyed
    // registries are exempt — their rows carry up-to-8KiB output tails, so a
    // whole-bag clone per journal row is quadratic in session history — and
    // report their own change instead.
    let keyed = matches!(
        tag,
        super::actions::WORK_BACKGROUND_TASKS | super::actions::WORK_SUBAGENTS
    );
    let before = (!keyed).then(|| state.clone());
    let mut registry_changed = false;
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
            // A whole-field replace would leave a late subscriber's registry
            // view holding only the last-changed task, so rows upsert by
            // `task_id`. A row without a string `task_id` cannot be keyed and
            // is dropped; the producer shape is pinned by the host's
            // `task_snapshot_serialization_is_wire_stable` golden test.
            if let Some(snapshot) = action.get("snapshot")
                && let Some(task_id) = snapshot.get("task_id").and_then(Value::as_str)
            {
                let registry = state.background_tasks.get_or_insert_with(BTreeMap::new);
                match registry.entry(task_id.to_string()) {
                    Entry::Occupied(slot) if slot.get() == snapshot => {}
                    Entry::Occupied(mut slot) => {
                        slot.insert(snapshot.clone());
                        registry_changed = true;
                    }
                    Entry::Vacant(slot) => {
                        slot.insert(snapshot.clone());
                        registry_changed = true;
                    }
                }
            }
        }
        super::actions::WORK_SUBAGENTS => {
            // Same registry shape as background tasks: progress rows carry
            // one agent's tick each, keyed upsert by `agentId`; a row
            // without a string `agentId` cannot be keyed and is dropped.
            if let Some(agent_id) = action.get("agentId").and_then(Value::as_str) {
                let mut row = action.clone();
                if let Some(fields) = row.as_object_mut() {
                    fields.remove("type");
                    let registry = state.subagents.get_or_insert_with(BTreeMap::new);
                    match registry.entry(agent_id.to_string()) {
                        Entry::Occupied(slot) if slot.get() == &row => {}
                        Entry::Occupied(mut slot) => {
                            slot.insert(row);
                            registry_changed = true;
                        }
                        Entry::Vacant(slot) => {
                            slot.insert(row);
                            registry_changed = true;
                        }
                    }
                }
            }
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
    if keyed {
        return if registry_changed {
            Outcome::Applied
        } else {
            Outcome::NoOp
        };
    }
    if before.as_ref() == Some(&*state) {
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

    #[test]
    fn background_task_rows_upsert_the_registry_view_by_task_id() {
        let snapshot = json!({"task_id": "bg_3", "status": "Running"});
        let action_for = |snapshot: Value| json!({"type": "x-manox-work/backgroundTasksChanged", "snapshot": snapshot});
        let mut state = XManoxState::default();
        assert_eq!(
            apply(&mut state, &action_for(snapshot.clone())),
            Outcome::Applied
        );
        assert_eq!(
            state.background_tasks,
            Some(
                [("bg_3".to_string(), snapshot.clone())]
                    .into_iter()
                    .collect()
            )
        );

        // A second task joins; a later tick of the first task replaces only
        // its own entry. The view is a registry, not last-writer-wins.
        let other = json!({"task_id": "mon_4", "status": "Running"});
        assert_eq!(apply(&mut state, &action_for(other)), Outcome::Applied);
        let tick = json!({"task_id": "bg_3", "status": "Completed"});
        assert_eq!(apply(&mut state, &action_for(tick)), Outcome::Applied);
        let registry = state
            .background_tasks
            .as_ref()
            .expect("registry view populated");
        assert_eq!(registry.len(), 2);
        assert_eq!(registry["bg_3"]["status"], "Completed");

        // A row without a keyable `task_id` cannot enter the view and does
        // not disturb it (there is no clear edge on this action).
        assert_eq!(
            apply(
                &mut state,
                &json!({"type": "x-manox-work/backgroundTasksChanged"})
            ),
            Outcome::NoOp
        );
        assert_eq!(
            apply(&mut state, &action_for(json!({"status": "Running"}))),
            Outcome::NoOp
        );
        assert_eq!(
            state
                .background_tasks
                .as_ref()
                .expect("still populated")
                .len(),
            2
        );
    }

    #[test]
    fn subagent_progress_rows_upsert_the_registry_view_by_agent_id() {
        let row_for = |agent: &str, status: &str| {
            json!({
                "type": "x-manox-work/subagentsChanged",
                "agentId": agent,
                "agentType": "explore",
                "status": status
            })
        };
        let mut state = XManoxState::default();
        assert_eq!(
            apply(&mut state, &row_for("sub-0", "running")),
            Outcome::Applied
        );
        assert_eq!(
            apply(&mut state, &row_for("sub-1", "running")),
            Outcome::Applied
        );
        assert_eq!(
            apply(&mut state, &row_for("sub-0", "completed")),
            Outcome::Applied
        );
        let registry = state.subagents.as_ref().expect("registry view populated");
        assert_eq!(registry.len(), 2, "each agent keeps its own entry");
        assert_eq!(registry["sub-0"]["status"], "completed");
        assert_eq!(
            registry["sub-0"].get("type"),
            None,
            "the envelope tag is stripped from stored rows"
        );

        // A row without an agent id cannot be keyed and does not disturb
        // the view.
        assert_eq!(
            apply(
                &mut state,
                &json!({"type": "x-manox-work/subagentsChanged", "status": "running"})
            ),
            Outcome::NoOp
        );
        assert_eq!(state.subagents.as_ref().expect("still populated").len(), 2);
    }

    /// The root-cause gate for the whole "hand-written key sets" class, in
    /// its routing-aware form: every host-emitted extension action must (a)
    /// be produced by the real translator, (b) ride an *extension* channel —
    /// an action on a state-bearing channel is `StateAction::Unknown` to that
    /// channel's reducer, so it is never folded and never baselined (the
    /// session-channel thread rows drifted exactly this way) — and (c) leave
    /// its state field populated. The final whole-struct comparison makes a
    /// *new* `XManoxState` field a compile error here until a producer case
    /// covers it, so "declared but always empty" cannot come back.
    #[test]
    fn every_extension_state_field_is_reachable_through_the_real_fold() {
        use manox_journal::{JournalWireEntry, JournalWireEvent};

        let entry = |event: JournalWireEvent| JournalWireEntry {
            seq: 0,
            id: "e-gate".into(),
            parent_id: None,
            timestamp: "2026-09-30T00:00:00.000Z".into(),
            event,
        };
        // Host-emitted: journal row → the tag it must produce.
        let host_cases: Vec<(JournalWireEvent, &str)> = vec![
            (
                JournalWireEvent::PlanModeChange { enabled: true },
                super::super::actions::PLAN_MODE_CHANGED,
            ),
            (
                JournalWireEvent::PlanUpdate {
                    snapshot: json!({}),
                },
                super::super::actions::PLAN_CHANGED,
            ),
            (
                JournalWireEvent::PlanReview {
                    state: "proposed".into(),
                    plan_file: None,
                    title: None,
                    content: None,
                },
                super::super::actions::PLAN_VERDICT_REQUESTED,
            ),
            (
                JournalWireEvent::Goal {
                    goal: Some(json!({"text": "ship"})),
                },
                super::super::actions::WORK_GOAL_CHANGED,
            ),
            (
                JournalWireEvent::BrowserSuites {
                    suites: vec!["chrome".into()],
                },
                super::super::actions::WORK_BROWSER_SUITES,
            ),
            (
                JournalWireEvent::BackgroundTask {
                    snapshot: json!({"task_id": "mon_7"}),
                },
                super::super::actions::WORK_BACKGROUND_TASKS,
            ),
            (
                JournalWireEvent::SubagentProgress {
                    agent_id: "sub-0".into(),
                    agent_type: "explore".into(),
                    tool_uses: 0,
                    latest_activity: None,
                    status: "running".into(),
                },
                super::super::actions::WORK_SUBAGENTS,
            ),
            (
                JournalWireEvent::ActiveToolsChange {
                    tools: vec!["bash".into()],
                },
                super::super::actions::WORK_ACTIVE_TOOLS,
            ),
            (
                JournalWireEvent::Metrics {
                    kind: "token_usage".into(),
                    data: json!({}),
                },
                super::super::actions::METRICS_CHANGED,
            ),
            (
                JournalWireEvent::PinnedArchived {
                    pinned: true,
                    archived: false,
                },
                super::super::actions::PINNED_CHANGED,
            ),
            (
                JournalWireEvent::Label { label: "x".into() },
                super::super::actions::LABEL_CHANGED,
            ),
            (
                JournalWireEvent::SessionInfo {
                    data: json!({"name": "agent"}),
                },
                super::super::actions::SESSION_INFO_CHANGED,
            ),
            (
                JournalWireEvent::Leaf {
                    target_id: "e-9".into(),
                },
                super::super::actions::LEAF_CHANGED,
            ),
        ];
        // Client-dispatched: the action as the client sends it (no journal
        // producer exists; the runtime dispatch arms accept these directly).
        let client_cases: Vec<(serde_json::Value, &str)> = vec![(
            json!({"type": super::super::actions::ORDER_CHANGED, "order": {"s-1": 1}}),
            super::super::actions::ORDER_CHANGED,
        )];
        // Excluded, no fold arm to feed: BASELINE (host envelope),
        // WORK_BACKGROUND_TASK_STOPPED (declared, never produced),
        // PLAN_VERDICT (folds with the host-emitted verdict-requested edge),
        // the workspaces rows (declared, fold lives outside this bag).

        let mut state = XManoxState::default();
        let mut plan_review_action = None;
        for (event, tag) in &host_cases {
            assert!(
                super::super::actions::ALL.contains(tag),
                "{tag} must stay declared"
            );
            let emitted =
                crate::translate::Translator::new().on_entry("c-1", "s-1", &entry(event.clone()));
            let hit = emitted
                .iter()
                .find(|e| serde_json::to_value(&e.action).unwrap()["type"] == *tag)
                .unwrap_or_else(|| panic!("{tag} has no translator producer"));
            let action = serde_json::to_value(&hit.action).unwrap();
            assert!(
                crate::ext::is_extension_channel(&hit.channel),
                "{tag} must ride an extension channel; it was emitted on {} where no fold runs",
                hit.channel
            );
            assert_eq!(
                apply(&mut state, &action),
                Outcome::Applied,
                "{tag} did not fold: {action}"
            );
            if *tag == super::super::actions::PLAN_VERDICT_REQUESTED {
                plan_review_action = Some(action.clone());
            }
        }
        for (action, tag) in &client_cases {
            assert_eq!(
                apply(&mut state, action),
                Outcome::Applied,
                "{tag} did not fold: {action}"
            );
        }

        // Total coverage: every field of the bag, populated by the cases
        // above. The struct literal has no `..Default` on purpose.
        assert_eq!(
            state,
            XManoxState {
                plan_mode: Some(true),
                plan: Some(json!({})),
                plan_review: plan_review_action,
                goal: Some(json!({"text": "ship"})),
                browser_suites: Some(vec!["chrome".to_string()]),
                background_tasks: Some(
                    [("mon_7".to_string(), json!({"task_id": "mon_7"}),)]
                        .into_iter()
                        .collect()
                ),
                subagents: Some(
                    [(
                        "sub-0".to_string(),
                        json!({
                            "agentId": "sub-0", "agentType": "explore",
                            "toolUses": 0, "latestActivity": null,
                            "status": "running"
                        }),
                    )]
                    .into_iter()
                    .collect()
                ),
                pinned: Some(true),
                label: Some("x".to_string()),
                session_info: Some(json!({"name": "agent"})),
                leaf: Some("e-9".to_string()),
                active_tools: Some(vec!["bash".to_string()]),
                order: Some(json!({"s-1": 1})),
            },
            "every extension-state field must be reachable from a live producer"
        );
    }
}
