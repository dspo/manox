//! Journal replay (L10, §C.3): the deterministic fold from one session's
//! active entry chain to the thread's journal-backed observable state.
//!
//! This fold is the authority behind the engine's restore path: every field a
//! §C.2 state-change entry carries rebuilds from the chain, and the chain is
//! the only source — replay reads the journal and nothing else. The
//! match below is exhaustive — a new kernel entry variant must classify
//! here (state-bearing or ignored) before it compiles, mirroring the
//! projection fold's coverage gate.
//!
//! Determinism: the same records always produce the same state — higher
//! seq wins per field (last entry in chain order), no wall clock and no
//! IO inside the fold. That is the replay half of the L10 gate; the
//! regression half (disk reload == live memory) lives in the engine tests.

use manox_harness::session::Event;
use manox_harness::session::jsonl::JournalRecord;
use manox_harness::types::AgentMessage;

/// The journal-backed observable state of one thread, folded from its
/// active chain. Every field is `None` while the chain carries no entry for
/// it — the journal is the only source of replay state, so a field with no
/// entry has no journaled value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplayedThreadState {
    /// Display title from the last `title` entry.
    pub title: Option<String>,
    /// Pin flag from the last `pinned_archived` entry.
    pub pinned: Option<bool>,
    /// Archive flag from the last `pinned_archived` entry.
    pub archived: Option<bool>,
    /// Durable user annotations from the `annotation_set` /
    /// `annotation_removed` entries, latest-wins per annotation id (`None`
    /// after a removal, so tombstones survive the pure fold).
    pub annotations: std::collections::BTreeMap<String, Option<serde_json::Value>>,
    /// Project binding from the last `project_change` entry;
    /// `Some(None)` is an explicit unbind.
    pub project: Option<Option<String>>,
    /// Snapshot value from the last `plan_update` entry (an empty snapshot
    /// is the model's cleared plan, not an absent one).
    pub plan_snapshot: Option<serde_json::Value>,
    /// Goal value from the last `goal` entry; `Some(Null)` is an explicit
    /// clear.
    pub goal: Option<serde_json::Value>,
    /// Effective working directory from the last `cwd_change` entry.
    pub cwd: Option<String>,
    /// Access mode from the last `access_mode_change` entry (the raw wire
    /// string; the merge parses it against the closed vocabulary).
    pub access_mode: Option<String>,
    /// Reasoning effort from the last `thinking_level_change` entry (the
    /// raw wire string, `"off"` included; the merge parses it).
    pub reasoning_effort: Option<String>,
    /// The id of the turn this chain ends inside: a `turn_start` row (or a
    /// `user` message row) with no later `turn_finish`. Legacy chains carry
    /// no `turn_start` row at all, so the user row is the shape that catches
    /// a host that died mid-turn there. `None` means the last turn closed and
    /// the restore has nothing to reconcile.
    pub open_turn: Option<String>,
}

/// An approval or question request the chain never answered.
///
/// Restore re-parks these on the engine's gates so a verdict that outlives
/// the process can still settle and journal — the durable-park contract:
/// every joining owner can answer a card the journal still carries, not
/// only the process that was alive when the park was opened.
#[derive(Debug, Clone, PartialEq)]
pub struct UnsettledInteraction {
    /// The auth id a verdict must name (the gate's pending key, and the
    /// `authId` the AHP face seeds the wire card under).
    pub auth_id: String,
    /// The parked tool's name (`AskUserQuestion` for an ask).
    pub tool_name: String,
    /// The park's summary line, verbatim.
    pub summary: String,
    /// The parked input, verbatim.
    pub input: serde_json::Value,
}

/// Fold the `Approval`/`Question` request/decision pairing: a `request`
/// arms (a later request for the same id replaces the earlier one), a
/// `decision` discharges, and a decision for an id the chain never
/// requested is a foreign row that changes nothing. The survivors are the
/// unsettled parks, in chain order. Approvals and questions come back
/// separately — the two gates keep distinct registries.
pub fn unsettled_interactions(
    records: &[JournalRecord],
) -> (Vec<UnsettledInteraction>, Vec<UnsettledInteraction>) {
    fn track(
        parks: &mut Vec<UnsettledInteraction>,
        kind: &str,
        auth_id: &str,
        payload: &serde_json::Value,
    ) {
        match kind {
            "request" => {
                parks.retain(|park| park.auth_id != auth_id);
                parks.push(UnsettledInteraction {
                    auth_id: auth_id.to_string(),
                    tool_name: payload
                        .get("toolName")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    summary: payload
                        .get("summary")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: payload
                        .get("input")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                });
            }
            "decision" => parks.retain(|park| park.auth_id != auth_id),
            _ => {}
        }
    }
    let mut approvals = Vec::new();
    let mut questions = Vec::new();
    for record in records {
        match &record.entry {
            Event::Approval {
                kind,
                auth_id,
                payload,
                ..
            } => track(&mut approvals, kind, auth_id, payload),
            Event::Question {
                kind,
                auth_id,
                payload,
                ..
            } => track(&mut questions, kind, auth_id, payload),
            _ => {}
        }
    }
    (approvals, questions)
}

/// Fold one active chain into the journal-backed state. Records must be in
/// chain order (the storage's `journal_range` order); the fold itself is a
/// pure last-wins scan.
pub fn replay_thread_state(records: &[JournalRecord]) -> ReplayedThreadState {
    let mut state = ReplayedThreadState::default();
    for record in records {
        match &record.entry {
            // ── journal-backed state ────────────────────────────────────
            Event::Title { title, .. } => {
                state.title = Some(title.clone());
            }
            Event::PinnedArchived {
                pinned, archived, ..
            } => {
                state.pinned = Some(*pinned);
                state.archived = Some(*archived);
            }
            // Durable user annotations: latest-wins per annotation id on the
            // replayed map — the same fold the notes cell renders from.
            Event::AnnotationSet { annotation, .. } => {
                if let Some(id) = annotation.get("id").and_then(serde_json::Value::as_str) {
                    state
                        .annotations
                        .insert(id.to_string(), Some(annotation.clone()));
                }
            }
            Event::AnnotationRemoved { annotation_id, .. } => {
                state.annotations.insert(annotation_id.clone(), None);
            }
            Event::ProjectChange { path, .. } => {
                state.project = Some(path.clone());
            }
            Event::PlanUpdate { snapshot, .. } => {
                state.plan_snapshot = Some(snapshot.clone());
            }
            Event::Goal { goal, .. } => {
                state.goal = Some(goal.clone().unwrap_or(serde_json::Value::Null));
            }
            Event::CwdChange { cwd, .. } => {
                state.cwd = Some(cwd.clone());
            }
            // ── session config (the journal rows are the durable authority):
            // last row wins, exactly the fold the AHP projection runs.
            Event::AccessModeChange { mode, .. } => {
                state.access_mode = Some(mode.clone());
            }
            Event::ThinkingLevelChange { thinking_level, .. } => {
                state.reasoning_effort = Some(thinking_level.clone());
            }
            // ── lifecycle: the chain's single open-turn fact ────────────
            // A turn opens on its `turn_start` row and — for the legacy
            // chains that carry no `turn_start` — on the user row that
            // begins it; a `turn_finish` closes whichever was open.
            Event::TurnStart { id, .. } => {
                state.open_turn = Some(id.clone());
            }
            Event::TurnFinish { .. } => {
                state.open_turn = None;
            }
            // ── transcript + display domain (rebuilt by the kernel's own
            //    transcript projection, not by this fold) ────────────────
            Event::Message {
                id,
                message: AgentMessage::User { .. },
                ..
            } => {
                state.open_turn = Some(id.clone());
            }
            Event::Message { .. }
            | Event::UiNote { .. }
            | Event::Custom { .. }
            | Event::CustomMessage { .. }
            | Event::Compaction { .. }
            | Event::CompactionStarted { .. }
            | Event::BranchSummary { .. } => {}
            // ── live-only or kernel-owned state: the active tool set rides
            //    `active_tools_change` through the kernel, and
            //    lifecycle/delta/metrics rows carry no persistable thread
            //    state ────────────────────────────────────────────────────
            //
            // The model row is here too, deliberately: the harness's own
            // restore projection reads it onto the agent, so this fold has
            // no second model answer to carry.
            Event::ModelChange { .. }
            | Event::ActiveToolsChange { .. }
            | Event::Stop { .. }
            | Event::Retry { .. }
            | Event::ErrorEvent { .. }
            | Event::AgentTextDelta { .. }
            | Event::AgentThinkingDelta { .. }
            | Event::ToolCall { .. }
            | Event::ToolResult { .. }
            | Event::ToolOutputChunk { .. }
            | Event::SubagentChild { .. }
            | Event::SubagentProgress { .. }
            | Event::BrowserSuites { .. }
            | Event::BackgroundTask { .. }
            | Event::Approval { .. }
            | Event::Question { .. }
            | Event::Label { .. }
            | Event::SessionInfo { .. }
            | Event::Leaf { .. }
            | Event::RunStarted { .. }
            | Event::RunSettled { .. }
            | Event::RunEscalated { .. }
            | Event::RunContinued { .. }
            | Event::RunGuided { .. }
            | Event::Metrics { .. } => {}
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use manox_harness::session::Event as E;

    fn record(entry: Event) -> JournalRecord {
        // The fold never reads seq; a dense counter keeps the fixtures
        // honest about chain order.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        JournalRecord {
            seq: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            entry,
        }
    }

    fn envelope() -> (String, Option<String>, chrono::DateTime<Utc>) {
        (uuid::Uuid::new_v4().to_string(), None, Utc::now())
    }

    fn park_entry(kind: &str, is_approval: bool, auth: &str) -> Event {
        let id = uuid::Uuid::new_v4().to_string();
        let (_, parent, ts) = envelope();
        let payload = serde_json::json!({
            "toolName": "TaskStop",
            "summary": "escalate",
            "input": {"task_id": "bg_0"},
        });
        if is_approval {
            E::Approval {
                id,
                parent_id: parent,
                timestamp: ts,
                kind: kind.into(),
                auth_id: auth.into(),
                payload,
            }
        } else {
            E::Question {
                id,
                parent_id: parent,
                timestamp: ts,
                kind: kind.into(),
                auth_id: auth.into(),
                payload,
            }
        }
    }

    #[test]
    fn unsettled_parks_pair_requests_with_decisions() {
        let records = vec![
            record(park_entry("request", true, "a1")),
            record(park_entry("decision", true, "a1")),
            record(park_entry("request", false, "q1")),
        ];
        let (approvals, questions) = unsettled_interactions(&records);
        assert!(approvals.is_empty(), "a decided approval is settled");
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].auth_id, "q1");
        assert_eq!(questions[0].tool_name, "TaskStop");
        assert_eq!(questions[0].summary, "escalate");
        assert_eq!(questions[0].input, serde_json::json!({"task_id": "bg_0"}));
    }

    #[test]
    fn a_repeated_request_replaces_and_a_foreign_decision_is_ignored() {
        let records = vec![
            record(park_entry("request", true, "a1")),
            record(park_entry("decision", true, "ghost")),
            record(park_entry("request", true, "a1")),
            record(park_entry("decision", true, "a1")),
            record(park_entry("request", true, "a2")),
        ];
        let (approvals, questions) = unsettled_interactions(&records);
        assert!(questions.is_empty());
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].auth_id, "a2");
    }

    #[test]
    fn fold_is_last_wins_per_field() {
        let (id, parent, ts) = envelope();
        let records = vec![
            record(E::Title {
                id: id.clone(),
                parent_id: parent,
                timestamp: ts,
                title: "first".into(),
            }),
            record(E::Title {
                id: uuid::Uuid::new_v4().to_string(),
                parent_id: Some(id.clone()),
                timestamp: ts,
                title: "second".into(),
            }),
            record(E::PinnedArchived {
                id: uuid::Uuid::new_v4().to_string(),
                parent_id: None,
                timestamp: ts,
                pinned: true,
                archived: false,
            }),
            record(E::PinnedArchived {
                id: uuid::Uuid::new_v4().to_string(),
                parent_id: None,
                timestamp: ts,
                pinned: false,
                archived: true,
            }),
        ];
        let state = replay_thread_state(&records);
        assert_eq!(state.title.as_deref(), Some("second"));
        assert_eq!(state.pinned, Some(false));
        assert_eq!(state.archived, Some(true));
    }

    /// The fold's open-turn fact, in both journal shapes: the chains with a
    /// `turn_start` row (engine-owned turns) and the legacy ones that carry
    /// only the user row (every prompt turn written before the engine owned
    /// the open edge).
    #[test]
    fn an_unclosed_turn_stays_open_and_a_finish_closes_it() {
        let ts = Utc::now();
        let id = || uuid::Uuid::new_v4().to_string();
        let user = |row_id: &str| E::Message {
            id: row_id.to_string(),
            parent_id: None,
            timestamp: ts,
            message: manox_harness::types::AgentMessage::user("hello"),
            origin: None,
            ui: None,
        };
        let finish = |row_id: &str| E::TurnFinish {
            id: row_id.to_string(),
            parent_id: None,
            timestamp: ts,
            cancelled: false,
            failed: false,
            stranded_steer_ids: Vec::new(),
            reason: None,
        };
        let start = |row_id: &str| E::TurnStart {
            id: row_id.to_string(),
            parent_id: None,
            timestamp: ts,
            owner: None,
        };

        // Engine-owned shape: the turn_start row is the open fact.
        let with_start = vec![
            record(user("u-1")),
            record(start("ts-1")),
            record(E::AgentTextDelta {
                id: id(),
                parent_id: None,
                timestamp: ts,
                delta: "partial".into(),
            }),
        ];
        assert_eq!(
            replay_thread_state(&with_start).open_turn.as_deref(),
            Some("ts-1")
        );

        // Legacy shape: no turn_start row at all — the user row is the open
        // fact, so a host that died mid-turn is still detected.
        let legacy = vec![record(user("u-legacy"))];
        assert_eq!(
            replay_thread_state(&legacy).open_turn.as_deref(),
            Some("u-legacy")
        );

        // A later finish closes whatever was open, and a chain that ends
        // closed reports none.
        let closed = [
            record(user("u-2")),
            record(start("ts-2")),
            record(finish("tf-2")),
        ];
        assert_eq!(replay_thread_state(&closed).open_turn, None);
        // A user row after the close opens the next turn.
        let reopened = [
            record(user("u-3")),
            record(finish("tf-3")),
            record(user("u-4")),
        ];
        assert_eq!(
            replay_thread_state(&reopened).open_turn.as_deref(),
            Some("u-4")
        );
    }

    #[test]
    fn every_state_entry_kind_folds() {
        let ts = Utc::now();
        let id = || uuid::Uuid::new_v4().to_string();
        let records = vec![
            record(E::ProjectChange {
                id: id(),
                parent_id: None,
                timestamp: ts,
                path: Some("/proj".into()),
            }),
            record(E::ThinkingLevelChange {
                id: id(),
                parent_id: None,
                timestamp: ts,
                thinking_level: "max".into(),
            }),
            record(E::PlanUpdate {
                id: id(),
                parent_id: None,
                timestamp: ts,
                snapshot: serde_json::json!([{"content": "step", "status": "pending", "activeForm": "stepping"}]),
            }),
            record(E::Goal {
                id: id(),
                parent_id: None,
                timestamp: ts,
                goal: Some(serde_json::json!({"objective": "ship"})),
            }),
            record(E::CwdChange {
                id: id(),
                parent_id: None,
                timestamp: ts,
                cwd: "/work".into(),
            }),
        ];
        let state = replay_thread_state(&records);
        assert_eq!(state.project, Some(Some("/proj".into())));
        assert!(state.plan_snapshot.is_some());
        assert_eq!(state.goal, Some(serde_json::json!({"objective": "ship"})));
        assert_eq!(state.cwd.as_deref(), Some("/work"));
    }
}
