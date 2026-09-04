//! Backend projection registry (architecture §E): the P face.
//!
//! Every hot session-level UI fact folds here, server-side, from the journal
//! (L3/L6): clients receive whole values per key with the seq that produced
//! them (`higher-seq-wins`), never recompute domain state. The declared
//! surface is [`manox_protocol::surface::PROJECTION_KEYS`] — the registry's
//! key set must match it exactly (tested; L12).
//!
//! [`ProjectionSet::apply`] is ONE exhaustive match over the entry
//! vocabulary: adding a journal kind without deciding its projection impact
//! is a compile error, not a forgotten code path.

use std::collections::BTreeMap;

use manox_agent::thread::{Thread, ThreadHandle};
use manox_harness::session::SessionTreeEntry;
use manox_harness::session::jsonl::JournalRecord;
use serde_json::Value as JsonValue;

/// One projection slot: the whole value plus the seq that last changed it.
#[derive(Debug)]
struct Slot {
    value: JsonValue,
    as_of_seq: u64,
    dirty: bool,
}

/// The per-session projection set (§E.2): seeded once from the live thread,
/// then folded forward by every journal record the follow pump applies.
#[derive(Debug)]
pub struct ProjectionSet {
    slots: BTreeMap<&'static str, Slot>,
}

impl ProjectionSet {
    /// Seed every declared key from the live thread (the snapshot baseline).
    pub fn seed(handle: &ThreadHandle) -> Self {
        handle.read(Self::seed_from)
    }

    /// Seed from a thread reference (test / replay entry point).
    pub fn seed_from(t: &Thread) -> Self {
        let permission = match t.permission_mode() {
            manox_harness::sandbox::PermissionMode::ReadOnly => "read_only",
            manox_harness::sandbox::PermissionMode::WorkspaceWrite => "workspace_write",
            manox_harness::sandbox::PermissionMode::DangerFullAccess => "danger_full_access",
        };
        let effort = match t.reasoning_effort() {
            manox_agent::language_model::ReasoningEffort::High => "high",
            manox_agent::language_model::ReasoningEffort::Max => "max",
        };
        let model = t
            .model()
            .map(|m| serde_json::json!({ "provider": m.provider, "modelId": m.id }));
        let slots = [
            ("title", serde_json::json!(t.display_title())),
            ("cwd", serde_json::json!(t.cwd().to_string_lossy())),
            (
                "project",
                t.project()
                    .map(|p| serde_json::json!(p.to_string_lossy()))
                    .unwrap_or(JsonValue::Null),
            ),
            ("model", model.unwrap_or(JsonValue::Null)),
            ("permission_mode", serde_json::json!(permission)),
            ("reasoning_effort", serde_json::json!(effort)),
            ("plan_mode", serde_json::json!(t.plan_mode())),
            (
                "plan",
                t.persisted_plan()
                    .and_then(|p| serde_json::to_value(p).ok())
                    .unwrap_or(JsonValue::Null),
            ),
            (
                "goal",
                t.goal()
                    .and_then(|g| serde_json::to_value(g).ok())
                    .unwrap_or(JsonValue::Null),
            ),
            ("running", serde_json::json!(t.is_running())),
            ("has_interacted", serde_json::json!(t.has_interacted())),
            ("pinned", serde_json::json!(t.is_pinned())),
            ("archived", serde_json::json!(t.archived())),
            ("depth", serde_json::json!(t.depth())),
            // No live getter carries the git branch into the session state
            // (the old Branch note came from the worktree poller); the
            // projection is fold-driven until that poller emits entries.
            ("branch", JsonValue::Null),
            (
                "browser_suites",
                serde_json::to_value(t.browser_suites()).unwrap_or(JsonValue::Null),
            ),
            ("pending_auth", serde_json::json!({})),
            ("background_tasks", serde_json::json!({})),
            ("agent_label", serde_json::json!(t.agent_label())),
            (
                "self_author",
                serde_json::to_value(t.self_author()).unwrap_or(JsonValue::Null),
            ),
        ];
        let slots = slots
            .into_iter()
            .map(|(key, value)| {
                (
                    key,
                    Slot {
                        value,
                        as_of_seq: 0,
                        dirty: false,
                    },
                )
            })
            .collect();
        Self { slots }
    }

    /// Fold one journal record. The exhaustive match is the coverage gate:
    /// a new vocabulary kind must be classified here to compile.
    pub fn apply(&mut self, record: &JournalRecord) {
        let seq = record.seq;
        match &record.entry {
            // ── transcript: the first user message is the has_interacted
            //    edge (the T1 bug's projection successor) ──────────────────
            SessionTreeEntry::Message { message, .. } => {
                if matches!(message, manox_harness::types::AgentMessage::User { .. }) {
                    self.set("has_interacted", JsonValue::Bool(true), seq);
                }
            }
            // ── lifecycle: the running projection's edges ─────────────────
            SessionTreeEntry::TurnStart { .. } => {
                self.set("running", JsonValue::Bool(true), seq);
            }
            SessionTreeEntry::TurnFinish { .. }
            | SessionTreeEntry::Stop { .. }
            | SessionTreeEntry::ErrorEvent { .. } => {
                self.set("running", JsonValue::Bool(false), seq);
            }
            // ── state changes ─────────────────────────────────────────────
            SessionTreeEntry::ModelChange {
                provider, model_id, ..
            } => {
                self.set(
                    "model",
                    serde_json::json!({ "provider": provider, "modelId": model_id }),
                    seq,
                );
            }
            SessionTreeEntry::CwdChange { cwd, .. } => {
                self.set("cwd", serde_json::json!(cwd), seq);
            }
            SessionTreeEntry::ProjectChange { path, .. } => {
                self.set(
                    "project",
                    path.as_ref()
                        .map(|p| serde_json::json!(p))
                        .unwrap_or(JsonValue::Null),
                    seq,
                );
            }
            SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } => {
                self.set("reasoning_effort", serde_json::json!(thinking_level), seq);
            }
            SessionTreeEntry::PermissionModeChange { mode, .. } => {
                self.set("permission_mode", serde_json::json!(mode), seq);
            }
            SessionTreeEntry::PlanModeChange { enabled, .. } => {
                self.set("plan_mode", serde_json::json!(enabled), seq);
            }
            SessionTreeEntry::PlanUpdate { snapshot, .. } => {
                self.set("plan", snapshot.clone(), seq);
            }
            SessionTreeEntry::Goal { goal, .. } => {
                self.set("goal", goal.clone().unwrap_or(JsonValue::Null), seq);
            }
            SessionTreeEntry::Title { title, .. } => {
                self.set("title", serde_json::json!(title), seq);
            }
            SessionTreeEntry::BrowserSuites { suites, .. } => {
                self.set("browser_suites", serde_json::json!(suites), seq);
            }
            SessionTreeEntry::PinnedArchived {
                pinned, archived, ..
            } => {
                self.set("pinned", serde_json::json!(pinned), seq);
                self.set("archived", serde_json::json!(archived), seq);
            }
            SessionTreeEntry::BranchSummary { summary, .. } => {
                self.set("branch", serde_json::json!(summary), seq);
            }
            SessionTreeEntry::BackgroundTask { snapshot, .. } => {
                // Whole-value merge keyed by task id — snapshots are
                // authoritative per task.
                if let Some(task_id) = snapshot.get("taskId").and_then(|v| v.as_str()) {
                    let mut map = self
                        .slots
                        .get("background_tasks")
                        .map(|s| s.value.clone())
                        .unwrap_or_else(|| serde_json::json!({}));
                    if let Some(obj) = map.as_object_mut() {
                        obj.insert(task_id.to_string(), snapshot.clone());
                    }
                    self.set("background_tasks", map, seq);
                }
            }
            SessionTreeEntry::Approval { kind, auth_id, .. } => {
                let mut map = self
                    .slots
                    .get("pending_auth")
                    .map(|s| s.value.clone())
                    .unwrap_or_else(|| serde_json::json!({}));
                if let Some(obj) = map.as_object_mut() {
                    match kind.as_str() {
                        "request" => {
                            obj.insert(auth_id.clone(), JsonValue::Bool(true));
                        }
                        "decision" => {
                            obj.remove(auth_id);
                        }
                        _ => {}
                    }
                }
                self.set("pending_auth", map, seq);
            }
            // ── no projection impact ──────────────────────────────────────
            // Streaming deltas (transcript domain), compaction boundaries
            // (transcript replace semantics), subagent streams (folded by the
            // UI from records), metrics (Q face), tree bookkeeping.
            SessionTreeEntry::AgentTextDelta { .. }
            | SessionTreeEntry::AgentThinkingDelta { .. }
            | SessionTreeEntry::ToolCall { .. }
            | SessionTreeEntry::ToolResult { .. }
            | SessionTreeEntry::ToolOutputChunk { .. }
            | SessionTreeEntry::SubagentChild { .. }
            | SessionTreeEntry::SubagentProgress { .. }
            | SessionTreeEntry::Retry { .. }
            | SessionTreeEntry::UiNote { .. }
            | SessionTreeEntry::Compaction { .. }
            | SessionTreeEntry::CompactionStarted { .. }
            | SessionTreeEntry::Metrics { .. }
            | SessionTreeEntry::Custom { .. }
            | SessionTreeEntry::CustomMessage { .. }
            | SessionTreeEntry::Label { .. }
            | SessionTreeEntry::SessionInfo { .. }
            | SessionTreeEntry::ActiveToolsChange { .. }
            | SessionTreeEntry::Leaf { .. } => {}
        }
    }

    /// The full baseline (snapshot payload, §D.1 `SessionSnapshot`).
    pub fn baseline(&self) -> BTreeMap<String, JsonValue> {
        self.slots
            .iter()
            .map(|(key, slot)| ((*key).to_string(), slot.value.clone()))
            .collect()
    }

    /// Drain changed keys since the last drain: `(as_of_seq, values)` where
    /// `as_of_seq` is the max producing seq (the ProjectionsFrame stamp).
    /// `None` when nothing changed.
    pub fn drain_changed(&mut self) -> Option<(u64, BTreeMap<String, JsonValue>)> {
        let dirty: Vec<&str> = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.dirty)
            .map(|(key, _)| *key)
            .collect();
        if dirty.is_empty() {
            return None;
        }
        let mut max_seq = 0u64;
        let mut values = BTreeMap::new();
        for key in dirty {
            if let Some(slot) = self.slots.get_mut(key) {
                slot.dirty = false;
                max_seq = max_seq.max(slot.as_of_seq);
                values.insert(key.to_string(), slot.value.clone());
            }
        }
        Some((max_seq, values))
    }

    fn set(&mut self, key: &'static str, value: JsonValue, seq: u64) {
        if let Some(slot) = self.slots.get_mut(key) {
            if slot.value != value {
                slot.value = value;
                slot.as_of_seq = seq;
                slot.dirty = true;
            } else if seq > slot.as_of_seq {
                // Same value re-derived later: no publish (idempotent), but
                // the stamp stays monotonic for the frame contract.
                slot.as_of_seq = seq;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{hermetic_home, init_globals, lock_globals};
    use manox_harness::types::AgentMessage;

    fn seeded() -> ProjectionSet {
        let _guard = lock_globals();
        hermetic_home();
        init_globals();
        ProjectionSet::seed(&Thread::landing(std::path::PathBuf::from("/tmp")))
    }

    fn record(seq: u64, e: SessionTreeEntry) -> JournalRecord {
        JournalRecord { seq, entry: e }
    }

    fn turn_start() -> SessionTreeEntry {
        SessionTreeEntry::TurnStart {
            id: "x".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn registry_keys_exactly_match_the_declared_surface() {
        let set = seeded();
        let mut got: Vec<String> = set.slots.keys().map(|k| k.to_string()).collect();
        let mut want: Vec<String> = manox_protocol::surface::PROJECTION_KEYS
            .iter()
            .map(|k| k.to_string())
            .collect();
        // The slots map is sorted (BTreeMap); compare as sets.
        got.sort();
        want.sort();
        assert_eq!(got, want, "the registry IS the declared surface (L12)");
    }

    #[test]
    fn first_user_message_flips_has_interacted_exactly_once() {
        let mut set = seeded();
        assert_eq!(set.baseline()["has_interacted"], JsonValue::Bool(false));

        let user = record(
            0,
            SessionTreeEntry::Message {
                id: "m1".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                message: AgentMessage::user("hello"),
                origin: None,
            },
        );
        set.apply(&user);
        assert_eq!(set.baseline()["has_interacted"], JsonValue::Bool(true));
        let (seq, changed) = set.drain_changed().expect("one dirty key");
        assert_eq!(seq, 0);
        assert!(changed.contains_key("has_interacted"));
        // A second user message does not re-publish (idempotent).
        set.apply(&user);
        assert!(set.drain_changed().is_none());
    }

    #[test]
    fn running_follows_lifecycle_edges() {
        let mut set = seeded();
        let mk = |e: SessionTreeEntry| record(1, e);
        set.apply(&mk(turn_start()));
        assert_eq!(set.baseline()["running"], JsonValue::Bool(true));
        set.apply(&mk(SessionTreeEntry::TurnFinish {
            id: "x".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            cancelled: false,
            failed: false,
            stranded_steer_ids: vec![],
        }));
        assert_eq!(set.baseline()["running"], JsonValue::Bool(false));
        // Error/Stop also clear a stuck running edge.
        set.apply(&mk(turn_start()));
        set.apply(&mk(SessionTreeEntry::ErrorEvent {
            id: "x".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: "boom".into(),
        }));
        assert_eq!(set.baseline()["running"], JsonValue::Bool(false));
    }

    #[test]
    fn pending_auth_folds_request_decision_pairs() {
        let mut set = seeded();
        let approval = |kind: &str, seq: u64| {
            record(
                seq,
                SessionTreeEntry::Approval {
                    id: "x".into(),
                    parent_id: None,
                    timestamp: chrono::Utc::now(),
                    kind: kind.into(),
                    auth_id: "auth-1".into(),
                    payload: serde_json::json!({}),
                },
            )
        };
        set.apply(&approval("request", 3));
        assert_eq!(
            set.baseline()["pending_auth"]["auth-1"],
            JsonValue::Bool(true)
        );
        set.apply(&approval("decision", 4));
        assert!(
            set.baseline()["pending_auth"]
                .as_object()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn model_change_carries_the_canonical_pair() {
        let mut set = seeded();
        set.apply(&record(
            5,
            SessionTreeEntry::ModelChange {
                id: "x".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                provider: "DeepSeek-anthropic".into(),
                model_id: "deepseek-chat".into(),
            },
        ));
        assert_eq!(
            set.baseline()["model"],
            serde_json::json!({"provider": "DeepSeek-anthropic", "modelId": "deepseek-chat"})
        );
        let (seq, changed) = set.drain_changed().unwrap();
        assert_eq!(seq, 5);
        assert!(changed.contains_key("model"));
    }
}
