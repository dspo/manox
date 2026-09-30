//! Journal replay (L10, §C.3): the deterministic fold from one session's
//! active entry chain to the thread's journal-backed observable state.
//!
//! This fold is the authority behind the engine's restore path (K2): every
//! field a §C.2 state-change entry carries rebuilds from the chain, and the
//! session sidecar is a derived cache that only fills fields the chain has
//! never seen (legacy journals predate the decision-point entries). The
//! match below is exhaustive — a new kernel entry variant must classify
//! here (state-bearing or ignored) before it compiles, mirroring the
//! projection fold's coverage gate.
//!
//! Determinism: the same records always produce the same state — higher
//! seq wins per field (last entry in chain order), no wall clock and no
//! IO inside the fold. That is the replay half of the L10 gate; the
//! regression half (disk reload == live memory) lives in the engine tests.

use manox_harness::session::SessionTreeEntry;
use manox_harness::session::jsonl::JournalRecord;

use crate::language_model::ReasoningEffort;
use crate::thread::PermissionMode;

/// The journal-backed observable state of one thread, folded from its
/// active chain. Every field is `None` while the chain carries no entry
/// for it — the restore path falls back to the sidecar cache for exactly
/// those fields (K2 migration window: old journals never saw the
/// decision-point entries).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplayedThreadState {
    /// Display title from the last `title` entry.
    pub title: Option<String>,
    /// Pin flag from the last `pinned_archived` entry.
    pub pinned: Option<bool>,
    /// Archive flag from the last `pinned_archived` entry.
    pub archived: Option<bool>,
    /// Project binding from the last `project_change` entry;
    /// `Some(None)` is an explicit unbind.
    pub project: Option<Option<String>>,
    /// Permission mode from the last parseable `permission_mode_change`
    /// entry (an unparseable mode string is skipped, keeping the previous
    /// entry's value — the vocabulary is bounded but the fold stays
    /// fail-soft on a hand-edited journal).
    pub permission_mode: Option<PermissionMode>,
    /// Reasoning effort from the last `thinking_level_change` entry whose
    /// level parses as an effort (`"off"` and unknown levels are not
    /// user-facing efforts and leave the field untouched).
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Plan-mode flag from the last `plan_mode_change` entry.
    pub plan_mode: Option<bool>,
    /// Snapshot value from the last `plan_update` entry (an empty snapshot
    /// is the model's cleared plan, not an absent one).
    pub plan_snapshot: Option<serde_json::Value>,
    /// Pending flag from the last `plan_review` entry (`"proposed"` raises,
    /// any verdict clears). `None` = the chain never saw one — the sidecar
    /// hint stands (the pre-vocabulary hole-fill).
    pub plan_review_pending: Option<bool>,
    /// The open review's request id, minted from the last `proposed` row's
    /// own entry id (the request id IS `plan-review:<proposal entry id>`); a
    /// resolved row clears it. The journal is the carrier, so the restore
    /// survives a host restart without a sidecar.
    pub plan_review_request_id: Option<String>,
    /// Goal value from the last `goal` entry; `Some(Null)` is an explicit
    /// clear.
    pub goal: Option<serde_json::Value>,
    /// Effective working directory from the last `cwd_change` entry.
    pub cwd: Option<String>,
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
            SessionTreeEntry::Approval {
                kind,
                auth_id,
                payload,
                ..
            } => track(&mut approvals, kind, auth_id, payload),
            SessionTreeEntry::Question {
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
            // ── journal-backed state (K2 authority) ─────────────────────
            SessionTreeEntry::Title { title, .. } => {
                state.title = Some(title.clone());
            }
            SessionTreeEntry::PinnedArchived {
                pinned, archived, ..
            } => {
                state.pinned = Some(*pinned);
                state.archived = Some(*archived);
            }
            SessionTreeEntry::ProjectChange { path, .. } => {
                state.project = Some(path.clone());
            }
            SessionTreeEntry::PermissionModeChange { mode, .. } => {
                // The closed kebab vocabulary (`from_wire`): an unknown
                // string leaves the previous entry in place.
                if let Some(parsed) = PermissionMode::from_wire(mode) {
                    state.permission_mode = Some(parsed);
                }
            }
            SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } => {
                if let Some(effort) = crate::engine::parse_reasoning_effort(thinking_level) {
                    state.reasoning_effort = Some(effort);
                }
            }
            SessionTreeEntry::PlanModeChange { enabled, .. } => {
                state.plan_mode = Some(*enabled);
            }
            SessionTreeEntry::PlanUpdate { snapshot, .. } => {
                state.plan_snapshot = Some(snapshot.clone());
            }
            SessionTreeEntry::PlanReview {
                state: review,
                id,
                request_id,
                ..
            } => {
                let proposed = review == "proposed";
                state.plan_review_pending = Some(proposed);
                state.plan_review_request_id = if proposed {
                    Some(
                        request_id
                            .clone()
                            .unwrap_or_else(|| manox_journal::plan_review_request_id(id)),
                    )
                } else {
                    None
                };
            }
            SessionTreeEntry::Goal { goal, .. } => {
                state.goal = Some(goal.clone().unwrap_or(serde_json::Value::Null));
            }
            SessionTreeEntry::CwdChange { cwd, .. } => {
                state.cwd = Some(cwd.clone());
            }
            // ── transcript + display domain (rebuilt by the kernel's own
            //    transcript projection, not by this fold) ────────────────
            SessionTreeEntry::Message { .. }
            | SessionTreeEntry::UiNote { .. }
            | SessionTreeEntry::Custom { .. }
            | SessionTreeEntry::CustomMessage { .. }
            | SessionTreeEntry::Compaction { .. }
            | SessionTreeEntry::CompactionStarted { .. }
            | SessionTreeEntry::BranchSummary { .. } => {}
            // ── live-only or kernel-owned state: the model rides the
            //    kernel's own restore (`adopt_session_model`), the active
            //    tool set rides `active_tools_change` through the kernel,
            //    and lifecycle/delta/metrics rows carry no persistable
            //    thread state ────────────────────────────────────────────
            SessionTreeEntry::ModelChange { .. }
            | SessionTreeEntry::ActiveToolsChange { .. }
            | SessionTreeEntry::TurnStart { .. }
            | SessionTreeEntry::TurnFinish { .. }
            | SessionTreeEntry::Stop { .. }
            | SessionTreeEntry::Retry { .. }
            | SessionTreeEntry::ErrorEvent { .. }
            | SessionTreeEntry::AgentTextDelta { .. }
            | SessionTreeEntry::AgentThinkingDelta { .. }
            | SessionTreeEntry::ToolCall { .. }
            | SessionTreeEntry::ToolResult { .. }
            | SessionTreeEntry::ToolOutputChunk { .. }
            | SessionTreeEntry::SubagentChild { .. }
            | SessionTreeEntry::SubagentProgress { .. }
            | SessionTreeEntry::BrowserSuites { .. }
            | SessionTreeEntry::BackgroundTask { .. }
            | SessionTreeEntry::Approval { .. }
            | SessionTreeEntry::PlanModeRequest { .. }
            | SessionTreeEntry::Question { .. }
            | SessionTreeEntry::Label { .. }
            | SessionTreeEntry::SessionInfo { .. }
            | SessionTreeEntry::Leaf { .. }
            | SessionTreeEntry::Metrics { .. } => {}
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use manox_harness::session::SessionTreeEntry as E;

    fn record(entry: SessionTreeEntry) -> JournalRecord {
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

    fn park_entry(kind: &str, is_approval: bool, auth: &str) -> SessionTreeEntry {
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

    #[test]
    fn fields_without_entries_stay_none() {
        // The K2 fallback contract: a legacy chain with no decision-point
        // entries leaves every field `None` so the restore path reads the
        // sidecar cache instead of overwriting it with defaults.
        let state = replay_thread_state(&[]);
        assert_eq!(state, ReplayedThreadState::default());
        assert!(state.title.is_none() && state.pinned.is_none());
        assert!(state.permission_mode.is_none() && state.project.is_none());
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
            record(E::PermissionModeChange {
                id: id(),
                parent_id: None,
                timestamp: ts,
                mode: "danger-full-access".into(),
            }),
            record(E::ThinkingLevelChange {
                id: id(),
                parent_id: None,
                timestamp: ts,
                thinking_level: "max".into(),
            }),
            record(E::PlanModeChange {
                id: id(),
                parent_id: None,
                timestamp: ts,
                enabled: true,
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
        assert_eq!(
            state.permission_mode,
            Some(PermissionMode::DangerFullAccess)
        );
        assert_eq!(state.reasoning_effort, Some(ReasoningEffort::Max));
        assert_eq!(state.plan_mode, Some(true));
        assert!(state.plan_snapshot.is_some());
        assert_eq!(state.goal, Some(serde_json::json!({"objective": "ship"})));
        assert_eq!(state.cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn unparseable_mode_and_off_level_do_not_clobber() {
        let ts = Utc::now();
        let id = || uuid::Uuid::new_v4().to_string();
        let records = vec![
            record(E::PermissionModeChange {
                id: id(),
                parent_id: None,
                timestamp: ts,
                mode: "workspace-write".into(),
            }),
            record(E::PermissionModeChange {
                id: id(),
                parent_id: None,
                timestamp: ts,
                mode: "not-a-mode".into(),
            }),
            record(E::ThinkingLevelChange {
                id: id(),
                parent_id: None,
                timestamp: ts,
                thinking_level: "high".into(),
            }),
            record(E::ThinkingLevelChange {
                id: id(),
                parent_id: None,
                timestamp: ts,
                thinking_level: "off".into(),
            }),
        ];
        let state = replay_thread_state(&records);
        assert_eq!(state.permission_mode, Some(PermissionMode::WorkspaceWrite));
        assert_eq!(state.reasoning_effort, Some(ReasoningEffort::High));
    }
}
