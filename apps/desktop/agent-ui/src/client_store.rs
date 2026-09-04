//! Client-side mirror of a thread's state.
//!
//! v1 (§C.2-era): a pure projection of `ServerNote`s, the γ-1 data foundation.
//! **Retired at T10c (§D.6): the v1 `ServerNote` transcript/meta fold is
//! deleted.** The views no longer read `messages`/`display_entries` — those
//! fields existed only for the `ThreadHistory` / `Compaction` note arms that the
//! server stopped emitting in T10b; the surviving `ServerNote` surface is the
//! global registry / control set (`Ready`, `SessionCreated`, `SessionDisposed`,
//! `Models`, `ThreadsUpdated`, `Commands`, `Error`) plus the model-chat side
//! stream.
//!
//! v2 (T6, spec §F.2): the `SessionStore` of the architecture doc. The
//! transcript is the gap-free **journal window** (`window: Vec<JournalWireEntry>`,
//! maintained by the [`crate::journal_fold`] engine); the hot state is the
//! **projection face** (`projections`, higher-seq-wins, §E.1/E.2); the
//! optimistic UI is the **echo map**; the transport state is `status`. The
//! `display` vector is the positive fold of the window and is the sole render
//! source.

use std::collections::{HashMap, HashSet};

use manox_agent::ThreadId;
use manox_protocol::ServerNote;
use manox_protocol::journal::JournalWireEntry;
use serde_json::Value;

use crate::journal_translate;

/// A client-side projection of one thread's state. v2 fields are set by the
/// journal fold (`apply_window_change`, `merge_projection*`); the global note
/// surface by `apply_server_note`. `with` is the view's sanctioned read face.
pub struct ClientStore {
    pub id: ThreadId,
    pub display_title: String,
    pub model_id: Option<String>,
    pub model: Option<serde_json::Value>,
    pub permission_mode: manox_agent::thread::PermissionMode,
    pub reasoning_effort: manox_agent::language_model::ReasoningEffort,
    pub pinned: bool,
    pub archived: bool,
    pub depth: u32,
    pub agent_label: String,
    pub self_author: manox_agent::MessageAuthor,
    pub branch: Option<String>,
    pub goal: Option<Value>,
    pub plan_mode: bool,
    pub persisted_plan: Option<Value>,
    pub browser_suites: Vec<manox_agent::engine::BrowserSuite>,
    pub running: bool,
    pub has_interacted: bool,
    pub cwd: String,
    pub project: Option<String>,
    pub background_tasks: Vec<Value>,
    /// Latest per-request usage, folded from the durable assistant `message`
    /// row's `usage` payload (v2; §C.2 transcript group). Successor of the
    /// deleted `TokenUsage` note.
    pub last_token_usage: Option<UsageSnapshot>,
    /// Per-request usage keyed by the assistant `message` row id (v2:
    /// `message.usage` + `metrics{token_usage}` sidecars). Successor of the
    /// `UsageSnapshot` note's `per_request`.
    pub per_request_usage: HashMap<String, UsageSnapshot>,
    // §D.6 successor wired: the Q face (`GetConversationInfo`) fills these
    // on the committed edge (a message row landing in the window — per-turn
    // frequency, no timer debounce needed). Never synthesize totals from
    // the window client-side (L6).
    // stopped pushing them in T10b). Their §D.6 successor is the §E.3 Q-face
    // (`GetConversationInfo`) port; until then the context rail reads the
    // zero values. Never synthesize totals client-side from the window (L6).
    pub cumulative_usage: Option<UsageSnapshot>,
    pub per_model_usage: HashMap<String, UsageSnapshot>,
    pub cumulative_cost: f64,
    pub per_model_cost: HashMap<String, f64>,
    /// Pending adjudication ServerCall (Approve/AskUser) from the AgentServer,
    /// keyed by `auth_id`. The workspace uses the MsgId to send the reply.
    pub pending_auth: HashMap<String, manox_protocol::MsgId>,
    /// Pending plan-verdict ServerCall from the AgentServer, keyed by
    /// `plan_file`. The workspace uses the MsgId to send the verdict reply.
    pub pending_plan_verdict: HashMap<String, manox_protocol::MsgId>,
    // ── v2 (§F.2 SessionStore) ──────────────────────────────────────────
    /// The gap-free journal window: wire entries with dense seq, oldest
    /// first. The single source of the v2 `display` fold (§F.1 rule 1-4).
    pub window: Vec<JournalWireEntry>,
    /// Older records exist before the window head (truncated snapshot).
    pub window_has_more: bool,
    /// The positive fold of the window (§F.2 "display = 对 window 的通用
    /// UI fold"): messages interleaved with UI notes, derived by
    /// [`crate::journal_translate`] — never stored independently.
    pub display: Vec<manox_agent::db::HistoryEntry>,
    /// The projection face (§E): key → whole value + the seq that produced
    /// it. Higher-seq-wins on merge; each merge materializes the mirrored
    /// field above (the existing fields are the keys' home).
    pub projections: HashMap<String, ProjectionValue>,
    /// Optimistic echo bubbles keyed by the `Submit`/`Steer` `origin_rpc`
    /// (the local uuid). A durable user `message` row with the same
    /// `originRpc` retires the entry (§F.2 echo/retire).
    pub echo: HashMap<String, EchoEntry>,
    /// §D.5 host mirror: `SessionStatus` deltas applied under the monotonic
    /// rules (unread only rises until focus clears it, errored is a rising
    /// edge, running is latest). The thread-store sidebar badges read these.
    pub errored: bool,
    pub unread: bool,
    /// The set of auth ids currently awaiting a verdict, per the
    /// `pending_auth` projection (the snapshot-style set — replaces the v1
    /// per-event accumulation for display, spec T6-5).
    pub pending_auth_set: HashSet<String>,
    /// Whether the v2 journal stream is the sole render source (live
    /// `ThreadEvent`s emitted from the fold, rebuild driven off `display`).
    /// Flipped `true` at T10; the v1 note fold is deleted at T10c.
    pub stream_drives_render: bool,
}

/// Typed token usage breakdown — the client's own projection of the `usage`
/// payload on assistant `message` rows and `metrics` sidecars. (T10c: formerly
/// re-exported from the protocol crate's deleted `TokenUsageSnapshot`.)
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSnapshot {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

/// One projection slot (§E.1): the whole value plus the producing seq.
#[derive(Debug, Clone)]
pub struct ProjectionValue {
    pub value: Value,
    pub seq: u64,
}

/// A pending optimistic echo (the local bubble's identity; the durable row
/// retires it by `originRpc` correlation).
#[derive(Debug, Clone)]
pub struct EchoEntry {
    /// The submitted text (kept for diagnostics / dedupe).
    pub text: String,
}

/// The result of applying a `SessionStatus` delta under the §D.5 monotonic
/// mirror rules: which flags actually flipped, for the leaf to refresh the
/// sidebar badges.
#[derive(Debug, Clone, Copy)]
pub struct StatusMirror {
    pub running: Option<bool>,
    pub errored_set: bool,
    pub pending_auth: Option<bool>,
    pub pending_plan: Option<bool>,
    pub background_work: Option<bool>,
}

impl Default for ClientStore {
    fn default() -> Self {
        Self {
            id: ThreadId::default(),
            display_title: String::new(),
            model_id: None,
            model: None,
            permission_mode: manox_agent::thread::PermissionMode::default(),
            reasoning_effort: manox_agent::language_model::ReasoningEffort::default(),
            pinned: false,
            archived: false,
            depth: 0,
            agent_label: String::new(),
            self_author: manox_agent::MessageAuthor::default(),
            branch: None,
            goal: None,
            plan_mode: false,
            persisted_plan: None,
            browser_suites: Vec::new(),
            running: false,
            has_interacted: false,
            cwd: String::new(),
            project: None,
            background_tasks: Vec::new(),
            cumulative_usage: None,
            per_model_usage: HashMap::new(),
            last_token_usage: None,
            cumulative_cost: 0.0,
            per_model_cost: HashMap::new(),
            per_request_usage: HashMap::new(),
            pending_auth: HashMap::new(),
            pending_plan_verdict: HashMap::new(),
            window: Vec::new(),
            window_has_more: false,
            display: Vec::new(),
            projections: HashMap::new(),
            echo: HashMap::new(),
            errored: false,
            unread: false,
            pending_auth_set: HashSet::new(),
            // T10: the v2 journal stream drives rendering; the v1 note
            // fold was deleted at T10c (server emission deleted in T10b).
            stream_drives_render: true,
        }
    }
}

impl ClientStore {
    /// §J11 selector read face: the view's only sanctioned way to read store
    /// state — a single borrow through which the caller extracts exactly the
    /// values it renders. New views go through this; legacy direct field
    /// reads migrate on T8's schedule.
    pub fn with<R>(&self, f: impl FnOnce(&ClientStore) -> R) -> R {
        f(self)
    }

    // ── v2 folding ────────────────────────────────────────────────────────

    /// Fold one window change from the journal engine into the store.
    /// Returns `true` when the display sequence changed *structurally*
    /// (a rebuild signal); appends fold incrementally and return `false`
    /// (the live render path consumes the appended entries directly).
    /// Fold one §E.3 `GetConversationInfo` payload into the usage panel
    /// fields (mechanical transcription of the server's fold — L6).
    pub fn apply_conversation_info(&mut self, payload: &serde_json::Value) {
        let snap = |v: &serde_json::Value| UsageSnapshot {
            input: v.get("input").and_then(|x| x.as_u64()).unwrap_or(0),
            output: v.get("output").and_then(|x| x.as_u64()).unwrap_or(0),
            cache_creation: v.get("cacheWrite").and_then(|x| x.as_u64()).unwrap_or(0),
            cache_read: v.get("cacheRead").and_then(|x| x.as_u64()).unwrap_or(0),
        };
        if let Some(cu) = payload.get("cumulativeUsage") {
            self.cumulative_usage = Some(snap(cu));
        }
        if let Some(cost) = payload.get("cumulativeCost").and_then(|v| v.as_f64()) {
            self.cumulative_cost = cost;
        }
        self.per_model_usage.clear();
        self.per_model_cost.clear();
        if let Some(models) = payload.get("models").and_then(|v| v.as_array()) {
            for row in models {
                let key = format!(
                    "{}/{}",
                    row.get("provider").and_then(|v| v.as_str()).unwrap_or(""),
                    row.get("model").and_then(|v| v.as_str()).unwrap_or("")
                );
                self.per_model_usage.insert(key.clone(), snap(row));
            }
        }
        if let Some(costs) = payload.get("perModelCost").and_then(|v| v.as_object()) {
            for (key, cost) in costs {
                if let Some(c) = cost.as_f64() {
                    self.per_model_cost.insert(key.clone(), c);
                }
            }
        }
    }

    pub fn apply_window_change(&mut self, change: crate::journal_fold::WindowChange) -> bool {
        use crate::journal_fold::WindowChange as C;
        match change {
            C::Replace { entries, has_more } => {
                self.window = entries;
                self.window_has_more = has_more;
                self.rebuild_display();
                true
            }
            C::Prepend { entries, has_more } => {
                // The engine guarantees head-adjacency; prepend in order.
                self.window.splice(0..0, entries);
                self.window_has_more = has_more;
                self.rebuild_display();
                true
            }
            C::Append(entry) => {
                // Echo retirement rides the durable row (§F.2): a user
                // message whose originRpc matches a local echo retires it
                // (the optimistic bubble becomes canonical, no re-render).
                if let Some(origin) = journal_translate::user_origin_rpc(&entry) {
                    self.retire_echo(origin);
                }
                if let Some((id, usage)) = journal_translate::usage_sidecar_of(&entry) {
                    self.record_request_usage(&id, &usage);
                }
                self.record_message_usage(&entry);
                let items = journal_translate::history_entries_of(&entry);
                let replaces = matches!(
                    entry.event,
                    manox_protocol::JournalWireEvent::Compaction { .. }
                );
                if replaces {
                    // A compaction row is a transcript boundary: the display
                    // restarts at the summary + retained tail (the window
                    // keeps the full chain for replay).
                    self.display = items;
                } else {
                    self.display.extend(items);
                }
                self.window.push(entry);
                false
            }
        }
    }

    /// Rebuild `display` from the whole window (the §F.2 UI fold).
    pub fn rebuild_display(&mut self) {
        let mut out = Vec::new();
        // Snapshot the (id, usage) sidecars and message usages from an
        // immutable pass first, so the display pass can mutate the store.
        let mut usages: Vec<(String, Value)> = Vec::new();
        let mut message_usage: Vec<JournalWireEntry> = Vec::new();
        for record in &self.window {
            let items = journal_translate::history_entries_of(record);
            if matches!(
                record.event,
                manox_protocol::JournalWireEvent::Compaction { .. }
            ) {
                // Boundary: the projection restarts at the summary.
                out = Vec::new();
            }
            out.extend(items);
            if let Some((id, usage)) = journal_translate::usage_sidecar_of(record) {
                usages.push((id, usage));
            }
            message_usage.push(record.clone());
        }
        self.display = out;
        for record in &message_usage {
            self.record_message_usage(record);
        }
        for (id, usage) in usages {
            self.record_request_usage(&id, &usage);
        }
    }

    /// Merge a projection frame (§E.1): per key, higher-`seq`-wins; each
    /// accepted value materializes into its mirrored field.
    pub fn merge_projections(&mut self, frame: &manox_protocol::ProjectionsFrame) {
        for (key, value) in &frame.values {
            self.merge_projection(key, value.clone(), frame.as_of_seq);
        }
    }

    /// The snapshot's full projection baseline (§D.1): same higher-seq-wins
    /// merge at `projections_as_of_seq`.
    pub fn merge_projection_baseline(
        &mut self,
        projections: &std::collections::BTreeMap<String, Value>,
        as_of_seq: u64,
    ) {
        for (key, value) in projections {
            self.merge_projection(key, value.clone(), as_of_seq);
        }
    }

    /// Merge one projection key with the higher-seq-wins rule.
    pub fn merge_projection(&mut self, key: &str, value: Value, seq: u64) {
        match self.projections.get_mut(key) {
            Some(slot) if slot.seq > seq => return,
            Some(slot) => {
                slot.value = value.clone();
                slot.seq = seq;
            }
            None => {
                self.projections.insert(
                    key.to_string(),
                    ProjectionValue {
                        value: value.clone(),
                        seq,
                    },
                );
            }
        }
        self.materialize_projection(key, &value);
    }

    /// Read a projection slot (the selector's P-face entry point).
    pub fn projection(&self, key: &str) -> Option<&ProjectionValue> {
        self.projections.get(key)
    }

    fn materialize_projection(&mut self, key: &str, value: &Value) {
        match key {
            "title" => self.display_title = value.as_str().unwrap_or_default().to_string(),
            "cwd" => self.cwd = value.as_str().unwrap_or_default().to_string(),
            "project" => {
                self.project = value.as_str().map(str::to_string);
            }
            "model" => {
                // L6/L8: display only the canonical wire identity; the human
                // label is not in the projection, so keep any existing name.
                self.model = Some(value.clone());
                self.model_id = value
                    .get("modelId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "permission_mode" => {
                let mode = value.as_str().unwrap_or_default();
                self.permission_mode =
                    serde_json::from_value(Value::String(mode.to_string())).unwrap_or_default();
            }
            "reasoning_effort" => {
                let effort = value.as_str().unwrap_or_default();
                self.reasoning_effort =
                    serde_json::from_value(Value::String(effort.to_string())).unwrap_or_default();
            }
            "plan_mode" => self.plan_mode = value.as_bool().unwrap_or(false),
            "plan" => self.persisted_plan = Some(value.clone()),
            "goal" => self.goal = Some(value.clone()),
            "running" => self.running = value.as_bool().unwrap_or(false),
            "has_interacted" => self.has_interacted = value.as_bool().unwrap_or(false),
            "pinned" => self.pinned = value.as_bool().unwrap_or(false),
            "archived" => self.archived = value.as_bool().unwrap_or(false),
            "depth" => self.depth = value.as_u64().unwrap_or(0) as u32,
            "branch" => self.branch = value.as_str().map(str::to_string),
            "browser_suites" => {
                self.browser_suites = value
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| serde_json::from_value(s.clone()).ok())
                            .collect()
                    })
                    .unwrap_or_default();
            }
            "pending_auth" => {
                self.pending_auth_set = value
                    .as_object()
                    .map(|obj| {
                        obj.iter()
                            .filter(|(_, v)| v.as_bool().unwrap_or(false))
                            .map(|(k, _)| k.clone())
                            .collect()
                    })
                    .unwrap_or_default();
            }
            "background_tasks" => {
                self.background_tasks = value
                    .as_object()
                    .map(|obj| obj.values().cloned().collect())
                    .unwrap_or_default();
            }
            "agent_label" => self.agent_label = value.as_str().unwrap_or_default().to_string(),
            "self_author" => {
                self.self_author = value
                    .as_str()
                    .map(manox_agent::MessageAuthor::from_routing)
                    .unwrap_or_default();
            }
            // Keys with no mirrored field yet (or display-only): the slot in
            // `projections` is still the truth; views select it directly.
            _ => {}
        }
    }

    fn record_message_usage(&mut self, entry: &JournalWireEntry) {
        if let manox_protocol::JournalWireEvent::Message {
            role,
            usage: Some(u),
            ..
        } = &entry.event
            && role == "assistant"
        {
            self.per_request_usage.insert(
                entry.id.clone(),
                UsageSnapshot {
                    input: u.input,
                    output: u.output,
                    cache_creation: u.cache_write,
                    cache_read: u.cache_read,
                },
            );
            self.last_token_usage = Some(UsageSnapshot {
                input: u.input,
                output: u.output,
                cache_creation: u.cache_write,
                cache_read: u.cache_read,
            });
        }
    }

    fn record_request_usage(&mut self, message_id: &str, usage: &Value) {
        let snapshot = UsageSnapshot {
            input: usage.get("input").and_then(Value::as_u64).unwrap_or(0),
            output: usage.get("output").and_then(Value::as_u64).unwrap_or(0),
            cache_creation: usage.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0),
            cache_read: usage.get("cacheRead").and_then(Value::as_u64).unwrap_or(0),
        };
        if snapshot.input + snapshot.output + snapshot.cache_read + snapshot.cache_creation > 0 {
            self.per_request_usage
                .insert(message_id.to_string(), snapshot);
        }
    }

    /// Mechanical transcription of the display fold's message rows (v2):
    /// the journal `message` rows ARE the messages (L6 — no client-side
    /// re-derivation; the window's compaction boundary is already folded).
    /// Replaces the v1 `messages` mirror for the transcript-rebuilding
    /// consumers (plan / subagent restore, agent-final-text lookup).
    pub fn derived_messages(&self) -> Vec<manox_agent::Message> {
        self.display
            .iter()
            .filter_map(|entry| match entry {
                manox_agent::db::HistoryEntry::Message(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    // ── echo (§F.2) ───────────────────────────────────────────────────────

    /// Register a local optimistic echo for `origin_rpc`.
    pub fn push_echo(&mut self, origin_rpc: &str, text: impl Into<String>) {
        self.echo
            .insert(origin_rpc.to_string(), EchoEntry { text: text.into() });
    }

    /// Retire the echo a durable row confirms; `true` when it existed.
    pub fn retire_echo(&mut self, origin_rpc: &str) -> bool {
        self.echo.remove(origin_rpc).is_some()
    }

    // ── §D.5 host-event mirror ────────────────────────────────────────────

    /// Apply one `SessionStatus` delta under the monotonic mirror rules:
    /// `running`/`pending_*`/`background_work` take the latest value;
    /// `unread` only rises (focus clears it), `errored` is a rising edge
    /// (a fresh turn or focus clears it). Returns the flags that flipped
    /// (so the leaf can refresh the sidebar badges).
    pub fn apply_session_status(
        &mut self,
        running: Option<bool>,
        errored: Option<bool>,
        unread: Option<bool>,
        pending_auth: Option<bool>,
        pending_plan: Option<bool>,
        background_work: Option<bool>,
    ) -> StatusMirror {
        if running == Some(true) {
            // A fresh turn clears the previous turn's error edge.
            self.errored = false;
        }
        if let Some(r) = running {
            self.running = r;
        }
        let errored_set = errored == Some(true) && !self.errored;
        if errored_set {
            self.errored = true;
        }
        let unread_set = unread == Some(true) && !self.unread;
        if unread_set {
            self.unread = true;
        }
        StatusMirror {
            running,
            errored_set: errored_set || unread_set,
            pending_auth,
            pending_plan,
            background_work,
        }
    }

    /// Focus clears the monotonic unread/errored mirror flags (§D.5).
    pub fn focus_cleared(&mut self) -> bool {
        let was = self.unread || self.errored;
        self.unread = false;
        self.errored = false;
        was
    }
}

impl ClientStore {
    /// Apply one retained `ServerNote` to the mirror. Post-T10c this is only
    /// the session-id binding; the registry/control notes are consumed by the
    /// multiplexer/views directly, the model-chat side stream bypasses the
    /// store, and every session-domain fact rides the v2 fold (window /
    /// projections / host status).
    pub fn apply_server_note(&mut self, note: &ServerNote) {
        // The session id is the thread id (`CreateSession` binds them); mirror
        // it so `store.id` matches the bound thread. Every other retained
        // note (registry/control list channel, `Error`, model-chat side
        // stream) is consumed by the multiplexer / views directly.
        if let ServerNote::SessionCreated { session_id } = note {
            self.id = ThreadId(session_id.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T10c: the v1 note-fold tests retired with the fold. Their coverage
    // moved to the v2 equivalents below / in the §F.2 section:
    //   thread_info_updates_all_fields   → projections_materialize_mirror_fields
    //   turn_started_finished_flip_running → running projection (same test)
    //   plan_mode_changed_updates        → projections_materialize_mirror_fields
    //   usage_snapshot_sets_cumulative   → assistant_usage_rows_fold_per_request
    //   compaction_note_replaces_store_transcript
    //                                     → compaction_row_restarts_display_fold

    #[test]
    fn session_created_note_binds_store_id() {
        let mut store = ClientStore::default();
        store.apply_server_note(&ServerNote::SessionCreated {
            session_id: "s1".into(),
        });
        assert_eq!(store.id.0, "s1");
        // Retained non-session notes are no-ops for the mirror.
        store.apply_server_note(&ServerNote::Ready);
        assert_eq!(store.id.0, "s1");
    }

    #[test]
    fn projections_materialize_mirror_fields() {
        let mut store = ClientStore::default();
        store.merge_projection(
            "permission_mode",
            Value::String("workspace-write".into()),
            1,
        );
        store.merge_projection("reasoning_effort", Value::String("high".into()), 1);
        store.merge_projection("plan_mode", Value::Bool(true), 1);
        store.merge_projection("running", Value::Bool(true), 1);
        store.merge_projection("depth", Value::from(2u64), 1);
        store.merge_projection("agent_label", Value::String("lead".into()), 1);
        store.merge_projection("self_author", Value::String("lead".into()), 1);
        store.merge_projection(
            "browser_suites",
            Value::Array(vec![Value::String("webexplore".into())]),
            1,
        );
        assert_eq!(
            store.permission_mode,
            manox_agent::thread::PermissionMode::WorkspaceWrite
        );
        assert_eq!(
            store.reasoning_effort,
            manox_agent::language_model::ReasoningEffort::High
        );
        assert!(store.plan_mode);
        assert!(store.running);
        assert_eq!(store.depth, 2);
        assert_eq!(store.agent_label, "lead");
        assert_eq!(store.self_author, manox_agent::MessageAuthor::Lead);
        assert_eq!(
            store.browser_suites,
            vec![manox_agent::engine::BrowserSuite::WebExplore]
        );
    }

    #[test]
    fn assistant_usage_rows_fold_per_request() {
        use manox_protocol::journal::UsagePayload;
        let mut store = ClientStore::default();
        store.apply_window_change(WindowChange::Append(JournalWireEntry {
            seq: 0,
            id: "m-9".into(),
            parent_id: None,
            timestamp: "2026-09-04T00:00:00.000Z".into(),
            event: manox_protocol::JournalWireEvent::Message {
                role: "assistant".into(),
                content: vec![],
                usage: Some(UsagePayload {
                    input: 100,
                    output: 50,
                    cache_read: 7,
                    cache_write: 3,
                    reasoning: 0,
                }),
                origin_rpc: None,
            },
        }));
        let snap = &store.per_request_usage["m-9"];
        assert_eq!(snap.input, 100);
        assert_eq!(snap.output, 50);
        assert_eq!(snap.cache_read, 7);
        assert_eq!(snap.cache_creation, 3);
        assert_eq!(store.last_token_usage.as_ref().unwrap().input, 100);
    }

    #[test]
    fn compaction_row_restarts_display_fold() {
        let mut store = ClientStore::default();
        // Seed a two-row transcript via window appends.
        store.apply_window_change(WindowChange::Append(JournalWireEntry {
            seq: 0,
            id: "m-0".into(),
            parent_id: None,
            timestamp: "2026-09-04T00:00:00.000Z".into(),
            event: manox_protocol::JournalWireEvent::Message {
                role: "user".into(),
                content: vec![serde_json::json!({"type": "text", "text": "hello"})],
                usage: None,
                origin_rpc: None,
            },
        }));
        store.apply_window_change(WindowChange::Append(JournalWireEntry {
            seq: 1,
            id: "m-1".into(),
            parent_id: Some("m-0".into()),
            timestamp: "2026-09-04T00:00:00.000Z".into(),
            event: manox_protocol::JournalWireEvent::Message {
                role: "assistant".into(),
                content: vec![serde_json::json!({"type": "text", "text": "world"})],
                usage: None,
                origin_rpc: None,
            },
        }));
        assert_eq!(store.derived_messages().len(), 2);
        // A compaction row is a transcript boundary: the display restarts at
        // the summary + retained tail (the window keeps the full chain).
        store.apply_window_change(WindowChange::Append(JournalWireEntry {
            seq: 2,
            id: "m-2".into(),
            parent_id: Some("m-1".into()),
            timestamp: "2026-09-04T00:00:00.000Z".into(),
            event: manox_protocol::JournalWireEvent::Compaction {
                summary: "compacted 1 message".into(),
                messages_compacted: 1,
                tokens_before: 10,
                retained_tail: vec![serde_json::json!({
                    "role": "assistant",
                    "content": [{"type": "text", "text": "world"}],
                })],
                first_kept_entry_id: None,
            },
        }));
        let msgs = store.derived_messages();
        // Summary carrier + retained assistant tail; the pre-compaction user
        // row leaves the display fold (the window still holds it).
        assert_eq!(msgs.len(), 2, "summary + retained tail");
        assert_eq!(
            msgs[1].role,
            manox_agent::language_model::Role::Assistant,
            "retained tail should survive compaction"
        );
        assert_eq!(store.window.len(), 3, "window keeps the full chain");
    }

    // ── v2 (§F.2) fold / projection / echo / host mirror ────────────────

    use crate::journal_fold::WindowChange;
    use manox_protocol::journal::JournalWireEvent as E;
    use std::collections::BTreeMap;

    fn wire(seq: u64, event: E) -> JournalWireEntry {
        JournalWireEntry {
            seq,
            id: format!("w-{seq}"),
            parent_id: None,
            timestamp: "2026-09-04T00:00:00.000Z".into(),
            event,
        }
    }

    #[test]
    fn fold_replace_then_append_builds_display() {
        let mut store = ClientStore::default();
        let snap = vec![wire(
            0,
            E::Message {
                role: "user".into(),
                content: vec![serde_json::json!({"type": "text", "text": "hi"})],
                usage: None,
                origin_rpc: None,
            },
        )];
        let rebuilt = store.apply_window_change(WindowChange::Replace {
            entries: snap,
            has_more: false,
        });
        assert!(rebuilt);
        assert_eq!(store.display.len(), 1);
        assert_eq!(store.window.len(), 1);
        store.apply_window_change(WindowChange::Append(wire(
            1,
            E::AgentTextDelta { s: "yo".into() },
        )));
        assert_eq!(store.window.len(), 2);
        // A pure delta has no display item but lands in the window.
        assert_eq!(store.display.len(), 1);
    }

    #[test]
    fn projections_higher_seq_wins_and_materializes() {
        let mut store = ClientStore::default();
        // Baseline at seq 10.
        store.merge_projection_baseline(
            &BTreeMap::from([("title".to_string(), Value::String("A".into()))]),
            10,
        );
        assert_eq!(store.display_title, "A");
        // A stale (lower-seq) projection must not clobber.
        store.merge_projection("title", Value::String("stale".into()), 5);
        assert_eq!(store.display_title, "A");
        // A newer projection wins.
        store.merge_projection("title", Value::String("B".into()), 12);
        assert_eq!(store.display_title, "B");
        assert_eq!(store.projection("title").unwrap().seq, 12);
    }

    #[test]
    fn append_user_row_with_origin_retires_echo() {
        let mut store = ClientStore::default();
        store.push_echo("rpc-77", "hello");
        let entry = wire(
            0,
            E::Message {
                role: "user".into(),
                content: vec![],
                usage: None,
                origin_rpc: Some("rpc-77".into()),
            },
        );
        store.apply_window_change(WindowChange::Append(entry));
        assert!(
            !store.echo.contains_key("rpc-77"),
            "echo retired by durable row"
        );
    }

    #[test]
    fn session_status_unread_monotonic_until_focus() {
        let mut store = ClientStore::default();
        // unread=true rises.
        store.apply_session_status(None, None, Some(true), None, None, None);
        assert!(store.unread);
        // A later unread=false from a status delta does not clear it (focus does).
        store.apply_session_status(None, None, Some(false), None, None, None);
        assert!(store.unread, "unread only clears on focus");
        // errored rising edge; a new turn (running=true) clears it.
        store.apply_session_status(None, Some(true), None, None, None, None);
        assert!(store.errored);
        store.apply_session_status(Some(true), Some(false), None, None, None, None);
        assert!(!store.errored, "a fresh turn clears the error edge");
        assert!(store.running);
        // focus clears the monotonic mirror flags.
        store.apply_session_status(None, None, Some(true), None, None, None);
        assert!(store.focus_cleared());
        assert!(!store.unread && !store.errored);
    }

    #[test]
    fn selector_with_reads_borrowed_view() {
        let mut store = ClientStore::default();
        store.merge_projection("running", Value::Bool(true), 1);
        let (running, title) = store.with(|s| (s.running, s.display_title.clone()));
        assert!(running);
        assert!(title.is_empty());
    }
}
