//! Client-side mirror of a thread's state.
//!
//! v1 (§C.2-era): a pure projection of `ServerNote`s, the γ-1 data foundation
//! — a pure projection with no gpui dependency, unit-testable headlessly.
//!
//! v2 (T6, spec §F.2): the same struct becomes the `SessionStore` of the
//! architecture doc. The transcript is now the gap-free **journal window**
//! (`window: Vec<JournalWireEntry>`, maintained by the [`crate::journal_fold`]
//! engine); the hot state is the **projection face** (`projections`,
//! higher-seq-wins, §E.1/E.2); the optimistic UI is the **echo map** (a local
//! bubble retired by the durable `message` row's `originRpc`); the transport
//! state is `status`. The v1 `ServerNote` fold stays wired through the
//! dual-protocol window (§K.5) — during the migration the server
//! double-sends notes *and* streams, and both paths land on the same fields
//! (the existing fields are the projection keys' home). The v2 `display`
//! vector is the positive fold of the window; at T10 the views switch to it
//! and `apply_server_note` retires.

use std::collections::{HashMap, HashSet};

use manox_agent::{Message, ThreadId};
use manox_protocol::ServerNote;
use manox_protocol::journal::JournalWireEntry;
use manox_protocol::server::{ThreadInfoPayload, TokenUsageSnapshot};
use serde_json::Value;

use crate::journal_translate;

/// A client-side projection of one thread's state. Every field is set by a
/// `ServerNote` (no recomputation). `apply_server_note` is the sole mutator.
pub struct ClientStore {
    pub id: ThreadId,
    pub messages: Vec<Message>,
    /// The `ThreadHistory` display payload parsed once at fold time; consumers
    /// clone the typed entries instead of re-parsing the raw `JsonValue` on
    /// every conversation rebuild (hot path for large transcripts).
    pub display_entries: Vec<manox_agent::db::HistoryEntry>,
    pub display_title: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub model: Option<serde_json::Value>,
    pub permission_mode: manox_agent::thread::PermissionMode,
    pub reasoning_effort: manox_agent::language_model::ReasoningEffort,
    pub pinned: bool,
    pub archived: bool,
    pub depth: u32,
    pub agent_label: String,
    pub self_author: manox_agent::MessageAuthor,
    pub cwd_path: Option<String>,
    pub branch: Option<String>,
    pub goal: Option<Value>,
    pub goal_elapsed_seconds: Option<u64>,
    pub plan_mode: bool,
    pub persisted_plan: Option<Value>,
    pub browser_suites: Vec<manox_agent::engine::BrowserSuite>,
    pub history_phase: manox_agent::thread::HistoryPhase,
    pub running: bool,
    pub has_interacted: bool,
    pub cwd: String,
    pub project: Option<String>,
    pub background_tasks: Vec<Value>,
    pub cumulative_usage: Option<TokenUsageSnapshot>,
    pub per_model_usage: HashMap<String, TokenUsageSnapshot>,
    pub last_token_usage: Option<TokenUsageSnapshot>,
    pub cumulative_cost: f64,
    pub per_model_cost: HashMap<String, f64>,
    pub per_request_usage: HashMap<String, TokenUsageSnapshot>,
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
    /// `false` through the dual-protocol window (§K.5) — the v1 `ServerNote`
    /// path still renders; flipped at T10 when the notes are deleted.
    pub stream_drives_render: bool,
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
            messages: Vec::new(),
            display_entries: Vec::new(),
            display_title: String::new(),
            model_id: None,
            model_name: None,
            model: None,
            permission_mode: manox_agent::thread::PermissionMode::default(),
            reasoning_effort: manox_agent::language_model::ReasoningEffort::default(),
            pinned: false,
            archived: false,
            depth: 0,
            agent_label: String::new(),
            self_author: manox_agent::MessageAuthor::default(),
            cwd_path: None,
            branch: None,
            goal: None,
            goal_elapsed_seconds: None,
            plan_mode: false,
            persisted_plan: None,
            browser_suites: Vec::new(),
            history_phase: manox_agent::thread::HistoryPhase::default(),
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
            stream_drives_render: false,
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
                TokenUsageSnapshot {
                    input: u.input,
                    output: u.output,
                    cache_creation: u.cache_write,
                    cache_read: u.cache_read,
                },
            );
            self.last_token_usage = Some(TokenUsageSnapshot {
                input: u.input,
                output: u.output,
                cache_creation: u.cache_write,
                cache_read: u.cache_read,
            });
        }
    }

    fn record_request_usage(&mut self, message_id: &str, usage: &Value) {
        let snapshot = TokenUsageSnapshot {
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
    /// Apply one `ServerNote`, updating the mirrored state.
    pub fn apply_server_note(&mut self, note: &ServerNote) {
        match note {
            ServerNote::ThreadInfo { info, .. } => self.apply_thread_info(info),
            // The session id is the thread id (`CreateSession` binds them);
            // mirror it so `store.id` matches the bound thread without
            // waiting for a `ThreadInfo` payload (which carries no id).
            ServerNote::SessionCreated { session_id } => {
                self.id = ThreadId(session_id.clone());
            }
            ServerNote::ThreadHistory {
                messages,
                display_history,
                ..
            } => {
                // WireMessage deflates Image.data to byte_len; the storage
                // Message ignores the extra byte_len (its Image.data is
                // #[serde(default)]) and accepts the identical field names,
                // so the typed payload round-trips into Vec<Message>.
                let value = serde_json::to_value(messages).unwrap_or_default();
                match serde_json::from_value::<Vec<Message>>(value) {
                    Ok(msgs) => self.messages = msgs,
                    Err(e) => {
                        tracing::warn!(error = %e, "ThreadHistory parse failed; keeping stale messages")
                    }
                }
                // Parse the display payload once here (fold time) instead of
                // on every conversation rebuild; a parse failure keeps the
                // previous entries rather than blanking the thread.
                match serde_json::from_value::<Vec<manox_agent::db::HistoryEntry>>(
                    display_history.clone(),
                ) {
                    Ok(entries) => self.display_entries = entries,
                    Err(e) => tracing::warn!(
                        error = %e,
                        "ThreadHistory display parse failed; keeping stale entries"
                    ),
                }
            }
            ServerNote::TurnStarted { .. } => self.running = true,
            ServerNote::TurnFinished { .. } => self.running = false,
            ServerNote::CurrentModel { id, name, .. } => {
                self.model_id = id.clone();
                self.model_name = name.clone();
            }
            ServerNote::PermissionModeChanged { mode, .. } => {
                self.permission_mode =
                    serde_json::from_value::<manox_agent::thread::PermissionMode>(Value::String(
                        mode.clone(),
                    ))
                    .unwrap_or_default();
            }
            ServerNote::ReasoningEffortChanged { effort, .. } => {
                self.reasoning_effort = serde_json::from_value::<
                    manox_agent::language_model::ReasoningEffort,
                >(Value::String(effort.clone()))
                .unwrap_or_default();
            }
            ServerNote::PlanModeChanged { enabled, .. } => self.plan_mode = *enabled,
            ServerNote::PlanUpdated { snapshot, .. } => self.persisted_plan = snapshot.clone(),
            ServerNote::GoalChanged { snapshot, .. } => self.goal = snapshot.clone(),
            ServerNote::CwdChanged { path, .. } => {
                self.cwd_path = Some(path.clone());
            }
            ServerNote::Branch { branch, .. } => self.branch = Some(branch.clone()),
            ServerNote::BrowserSuitesChanged { suites, .. } => {
                self.browser_suites = suites
                    .iter()
                    .filter_map(|s| {
                        serde_json::from_value::<manox_agent::engine::BrowserSuite>(Value::String(
                            s.clone(),
                        ))
                        .ok()
                    })
                    .collect();
            }
            ServerNote::BackgroundTaskUpdated { snapshot, .. } => {
                if let Some(obj) = snapshot.as_object()
                    && let Some(id) = obj.get("task_id").and_then(Value::as_str)
                {
                    if let Some(idx) = self
                        .background_tasks
                        .iter()
                        .position(|t| t.get("task_id").and_then(Value::as_str) == Some(id))
                    {
                        self.background_tasks[idx] = snapshot.clone();
                    } else {
                        self.background_tasks.push(snapshot.clone());
                    }
                }
            }
            ServerNote::UsageSnapshot {
                session_id: _,
                cumulative,
                per_model,
                cumulative_cost,
                per_model_cost,
                per_request,
            } => {
                self.cumulative_usage = Some(cumulative.clone());
                self.per_model_usage = per_model.clone();
                self.cumulative_cost = *cumulative_cost;
                self.per_model_cost = per_model_cost.clone();
                self.per_request_usage = per_request.clone();
            }
            ServerNote::TokenUsage {
                input,
                output,
                cache_creation,
                cache_read,
                ..
            } => {
                self.last_token_usage = Some(TokenUsageSnapshot {
                    input: *input,
                    output: *output,
                    cache_creation: *cache_creation,
                    cache_read: *cache_read,
                });
            }
            // A compaction notification replaces the store's transcript with
            // the retained tail (the server folds older history into the
            // summary and keeps the most recent messages).
            ServerNote::Compaction { retained, .. } => {
                match serde_json::from_value::<Vec<Message>>(retained.clone()) {
                    Ok(msgs) => self.messages = msgs,
                    Err(e) => {
                        tracing::warn!(error = %e, "Compaction retained parse failed; keeping stale messages")
                    }
                }
            }
            _ => {}
        }
    }

    fn apply_thread_info(&mut self, info: &ThreadInfoPayload) {
        self.cwd = info.cwd.clone();
        self.project = info.project.clone();
        self.display_title = info.display_title.clone();
        self.model_id = info.model_id.clone();
        self.model_name = info.model_name.clone();
        self.model = info.model.clone();
        self.permission_mode = serde_json::from_value::<manox_agent::thread::PermissionMode>(
            Value::String(info.permission_mode.clone()),
        )
        .unwrap_or_default();
        self.reasoning_effort = serde_json::from_value::<
            manox_agent::language_model::ReasoningEffort,
        >(Value::String(info.reasoning_effort.clone()))
        .unwrap_or_default();
        self.pinned = info.pinned;
        self.archived = info.archived;
        self.depth = info.depth;
        self.agent_label = info.agent_label.clone();
        self.self_author = manox_agent::MessageAuthor::from_routing(&info.self_author);
        self.cwd_path = info.cwd_path.clone();
        self.branch = info.branch.clone();
        self.goal = info.goal.clone();
        self.goal_elapsed_seconds = info.goal_elapsed_seconds;
        self.plan_mode = info.plan_mode;
        self.browser_suites = info
            .browser_suites
            .iter()
            .filter_map(|s| {
                serde_json::from_value::<manox_agent::engine::BrowserSuite>(Value::String(
                    s.clone(),
                ))
                .ok()
            })
            .collect();
        self.history_phase = serde_json::from_value::<manox_agent::thread::HistoryPhase>(
            Value::String(info.history_phase.clone()),
        )
        .unwrap_or_default();
        self.running = info.running;
        self.has_interacted = info.has_interacted;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_payload() -> ThreadInfoPayload {
        ThreadInfoPayload {
            cwd: "/proj".into(),
            project: None,
            display_title: "Test".into(),
            model_id: Some("claude-sonnet".into()),
            model_name: Some("Sonnet".into()),
            model: None,
            permission_mode: "workspace-write".into(),
            reasoning_effort: "high".into(),
            pinned: false,
            archived: false,
            depth: 0,
            agent_label: "lead".into(),
            self_author: "lead".into(),
            cwd_path: None,
            branch: None,
            goal: None,
            goal_elapsed_seconds: None,
            plan_mode: false,
            browser_suites: vec![],
            history_phase: "ready".into(),
            running: false,
            has_interacted: false,
        }
    }

    #[test]
    fn thread_info_updates_all_fields() {
        let mut store = ClientStore::default();
        store.apply_server_note(&ServerNote::ThreadInfo {
            session_id: "s1".into(),
            info: Box::new(sample_payload()),
        });
        assert_eq!(store.cwd, "/proj");
        assert_eq!(store.display_title, "Test");
        assert_eq!(store.model_id.as_deref(), Some("claude-sonnet"));
        assert_eq!(
            store.permission_mode,
            manox_agent::thread::PermissionMode::WorkspaceWrite
        );
        assert_eq!(
            store.reasoning_effort,
            manox_agent::language_model::ReasoningEffort::High
        );
        assert!(!store.running);
        assert!(!store.plan_mode);
        assert_eq!(store.self_author.routing(), "lead");
    }

    #[test]
    fn turn_started_finished_flip_running() {
        let mut store = ClientStore::default();
        store.apply_server_note(&ServerNote::TurnStarted {
            session_id: "s1".into(),
        });
        assert!(store.running);
        store.apply_server_note(&ServerNote::TurnFinished {
            session_id: "s1".into(),
            cancelled: false,
            failed: false,
            stranded_steer_ids: vec![],
        });
        assert!(!store.running);
    }

    #[test]
    fn plan_mode_changed_updates() {
        let mut store = ClientStore::default();
        store.apply_server_note(&ServerNote::PlanModeChanged {
            session_id: "s1".into(),
            enabled: true,
        });
        assert!(store.plan_mode);
    }

    #[test]
    fn usage_snapshot_sets_cumulative() {
        let mut store = ClientStore::default();
        store.apply_server_note(&ServerNote::UsageSnapshot {
            session_id: "s1".into(),
            cumulative: TokenUsageSnapshot {
                input: 100,
                output: 50,
                cache_creation: 0,
                cache_read: 0,
            },
            per_model: HashMap::new(),
            cumulative_cost: 0.01,
            per_model_cost: HashMap::new(),
            per_request: HashMap::new(),
        });
        assert_eq!(store.cumulative_usage.as_ref().unwrap().input, 100);
        assert!((store.cumulative_cost - 0.01).abs() < f64::EPSILON);
    }

    #[test]
    fn compaction_note_replaces_store_transcript() {
        let mut store = ClientStore::default();
        // Seed the store with a pair of messages via ThreadHistory.
        let msgs = vec![
            Message::user("hello".into()),
            Message::assistant(vec![manox_agent::language_model::MessageContent::Text(
                "world".into(),
            )]),
        ];
        let history = serde_json::to_value(&msgs).unwrap();
        let wire_messages: Vec<manox_protocol::WireMessage> =
            serde_json::from_value(history).expect("wire round-trip");
        store.apply_server_note(&ServerNote::ThreadHistory {
            session_id: "s1".into(),
            messages: wire_messages,
            display_history: serde_json::Value::Array(Vec::new()),
            auto_approved_tools: None,
            restored: false,
            loading: false,
        });
        assert_eq!(store.messages.len(), 2, "should have two seeded messages");
        // Apply a Compaction note with a summary and a retained tail that
        // keeps only the assistant message.
        let retained = serde_json::to_value(&msgs[1..]).unwrap();
        store.apply_server_note(&ServerNote::Compaction {
            session_id: "s1".into(),
            summary: "compacted 1 message".into(),
            retained,
        });
        // The store must replace (not append) its transcript: only the
        // retained tail remains, plus the compaction summary message.
        assert_eq!(
            store.messages.len(),
            1,
            "compaction should replace transcript, leaving only retained messages"
        );
        // The retained message is the assistant's "world" reply.
        assert_eq!(
            store.messages[0].role,
            manox_agent::language_model::Role::Assistant,
            "retained tail should survive compaction"
        );
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
