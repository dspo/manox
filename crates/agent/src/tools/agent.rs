//! The `agent` tool: spawn a sub-agent with its own context, restricted tools,
//! and system prompt, then return its final assistant message as the tool
//! result. Mirrors Claude Code's `Agent` tool pattern.
//!
//! Design:
//! - Each sub-agent is an independent `Entity<Thread>` with a fresh message
//!   list, a restricted `ToolRegistry`, and an independent `PermissionCache`.
//! - The parent-side `tool_use_id` (from `ToolOutputSink::tool_call_id`) is the
//!   stable key for snapshot storage and for composing bubbled-auth ids.
//! - The sub-agent's conversation is observed through its child `Thread` by the
//!   UI; `ToolCallAuthorization` is re-emitted
//!   on the parent under a composite id `<parent_tool_use_id>::<child_id>` so
//!   the existing approval overlay resolves it transparently, and the decision
//!   is routed back to the child.
//! - Nesting depth is capped at `MAX_DEPTH`; a sub-agent may only spawn its own
//!   sub-agents when its definition sets `allow_nesting: true`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gpui::{App, AppContext, AsyncApp, Task, WeakEntity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::agent_def::{self, AgentDefinition, AgentDefinitionFile};
use crate::language_model::{AnyLanguageModel, MessageContent, Role, StopReason, TokenUsage};
use crate::message::Message;
use crate::provider::registry;
use crate::thread::{self, Thread, ThreadEvent, ToolCallStatus};
use crate::tool::permission::PermissionCache;
use crate::tool::{AgentTool as AgentToolTrait, AnyAgentTool, ToolOutputSink, ToolRegistry};

/// Hard cap on sub-agent nesting depth. Main thread is depth 0; a sub-agent
/// spawned at depth `MAX_DEPTH` cannot itself register the `agent` tool.
const MAX_DEPTH: u32 = 5;

/// The `agent` tool. `parent` weakly references the `Thread` that owns this
/// tool so it can read the parent's model and route bubbled authorizations.
pub struct SpawnAgentTool {
    cwd: Arc<PathBuf>,
    depth: u32,
    parent: WeakEntity<Thread>,
    /// Description string built at construction time from the loaded agent
    /// definitions; advertised to the model so it knows which `subagent_type`
    /// values are available.
    desc: Arc<str>,
}

impl SpawnAgentTool {
    pub fn new(
        cwd: Arc<PathBuf>,
        depth: u32,
        parent: WeakEntity<Thread>,
        lang: crate::language::Language,
    ) -> Self {
        let desc = build_description(lang);
        Self {
            cwd,
            depth,
            parent,
            desc,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AgentToolInput {
    /// Name of the sub-agent definition to spawn (from `~/.config/cx/manox/agents/*.md`).
    subagent_type: String,
    /// The task to delegate. Becomes the sub-agent's first user message; the
    /// sub-agent has no access to the parent's conversation history, so include
    /// any needed file paths, error text, or context here.
    prompt: String,
    /// When `"worktree"`, the sub-agent runs in its own git worktree on a fresh
    /// branch — full filesystem isolation from the parent's working tree. The
    /// child's cwd is the worktree path; its sandbox confines writes to that
    /// worktree (the parent's project root is out of reach) while admitting the
    /// bound repo's `.git` and network for git ops. A clean worktree is
    /// auto-removed when the sub-agent finishes; a dirty one is left for
    /// inspection. Absent / `"none"` = share the parent's cwd (default).
    #[serde(default)]
    isolation: Option<String>,
    /// A short one-line title for the sub-agent task (e.g. "Review auth flow").
    /// Displayed in the UI as `"{subagent_type} · {description}"`. Separate from
    /// the full `prompt` so the status row stays compact.
    description: String,
}

impl AgentToolTrait for SpawnAgentTool {
    fn name(&self) -> &str {
        super::AGENT
    }

    fn description(&self) -> &str {
        &self.desc
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema::<AgentToolInput>()
    }

    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        false
    }

    /// The `agent` tool itself does not mutate the world — spawning a sub-agent
    /// is not a file write. The sub-agent's own write capability is governed by
    /// its definition's tool set, so plan mode treats `agent` as read-only and
    /// advertises it (via `to_request_tools_read_only`) so the main thread can
    /// delegate research to the bundled read-only `plan`/`explore` sub-agents.
    /// A write-capable user-authored sub-agent could still be spawned in plan
    /// mode — that escape is bounded by the sub-agent's own definition and is
    /// the same prompt-level trust Claude Code relies on, not a hard wall.
    fn is_read_only(&self) -> bool {
        true
    }

    fn run(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        // The non-streaming entry point delegates to `run_streaming` with a
        // discard sink. In practice the owning `Thread` always calls
        // `run_streaming` directly, so this path is rarely hit.
        let (sink, _rx) = ToolOutputSink::channel(Arc::from(""));
        let _ = _rx;
        self.run_streaming(input, cancel, sink, ctx, cx)
    }

    fn run_streaming(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        sink: ToolOutputSink,
        _ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let cwd = self.cwd.clone();
        let depth = self.depth;
        let parent = self.parent.clone();
        let ptu = sink.tool_call_id().to_string();

        // Entity creation, subscription, and turn start all need `&mut App`,
        // which is unavailable inside `cx.spawn`'s `&mut AsyncApp`. Resolve them
        // synchronously, then move the live handles into the spawned task that
        // awaits completion.
        let setup = match setup_child(&cwd, depth, &parent, &input, &ptu, cx) {
            Ok(s) => s,
            Err(e) => return cx.background_spawn(async move { Err(e) }),
        };

        let child = setup.child;
        let done_rx = setup.done_rx;
        let child_errored = setup.child_errored;
        let child_error_text = setup.child_error_text;
        let metrics = setup.metrics;
        let sub = setup.sub;

        cx.spawn(async move |cx: &mut AsyncApp| {
            // Wait for the sub-agent to finish or for the parent's turn to be
            // cancelled. `done_rx` resolves on `Stop`; if the subscription is
            // dropped first (e.g. child entity released), `recv` errors out.
            let cancelled = tokio::select! {
                recv = done_rx.recv() => recv.is_err(),
                _ = cancel.cancelled() => true,
            };
            if cancelled {
                child.update(cx, |c, cx| {
                    c.cancel(cx);
                });
            }

            // Capture the sub-agent's conversation (bounded for the envelope)
            // and extract its final text as the tool result. A crashed child
            // reports its error as the final text so the parent model sees an
            // actionable failure, not a bare sentinel.
            let msgs = bound_envelope_messages(child.read_with(cx, |c, _| c.messages().to_vec()));
            let error_text = child_error_text
                .lock()
                .expect("subagent error text poisoned")
                .clone();
            let final_text = envelope_final_text(&msgs, error_text);
            let errored = child_errored.load(std::sync::atomic::Ordering::Relaxed)
                || final_text == NO_FINAL_SENTINEL;
            drop(sub);

            // Stamp the terminal status into the telemetry snapshot so the
            // persisted envelope carries the final verdict (the live
            // `SubagentProgress` stream already relayed status transitions while
            // the child ran; this pins the last one).
            let final_metrics = {
                let mut acc = metrics.lock().expect("subagent metrics poisoned");
                acc.metrics.status = Some(if errored {
                    ToolCallStatus::Error
                } else {
                    ToolCallStatus::Success
                });
                acc.metrics.clone()
            };

            // The tool result is a JSON envelope {"final":..., "messages":[...],
            // "metrics":...}. The envelope in the parent's ToolResult is the
            // single source of truth: build_completion_request strips it to
            // `final` for the model (context isolation), and the UI parses
            // `messages` / `metrics` from it for the read-only observation
            // panel — both live and after reload. The UI's live registry is
            // scoped to the active main task and can be rebuilt from this
            // envelope after it is released.
            //
            // Returning the envelope as `Err` (rather than `Ok`) when the
            // sub-agent errored or produced no final message routes through
            // `Thread::run_tool_inner`'s `Err(e) => (e, true)` arm — the ToolResult
            // keeps the envelope as its content (so the UI panel and the
            // `agent_final_text`/`agent_sub_messages`/`agent_metrics` parsers
            // still work) but is flagged `is_error: true`, so the parent model
            // sees the failure instead of mistaking a non-reply for success
            // (thread 6cd3d096).
            let payload = serde_json::json!({
                "final": final_text,
                "messages": msgs,
                "metrics": final_metrics,
            });
            if errored {
                Err(payload.to_string())
            } else {
                Ok(payload.to_string())
            }
        })
    }
}

/// Everything `run_streaming` builds synchronously before spawning the await
/// task. `child` and `sub` are moved into that task; `done_rx` resolves when
/// the sub-agent emits `Stop`; `child_errored` is set by the subscription when
/// the sub-agent emits `ThreadEvent::Error` (with the error text stashed in
/// `child_error_text` for the envelope's failure summary); `metrics`
/// accumulates the child's tool-use / token / activity telemetry so
/// `run_streaming` can stamp the final snapshot into the result envelope.
struct SubagentSetup {
    child: gpui::Entity<Thread>,
    done_rx: async_channel::Receiver<()>,
    child_errored: Arc<AtomicBool>,
    child_error_text: Arc<std::sync::Mutex<Option<String>>>,
    metrics: Arc<std::sync::Mutex<ProgressAccumulator>>,
    sub: gpui::Subscription,
}

/// Transient accumulator for one sub-agent's UI telemetry. `seen_tools` dedupes
/// the child's `ToolCall` status-transition events so `tool_uses` counts distinct
/// tool invocations, not status changes. `metrics` is the persistable snapshot.
struct ProgressAccumulator {
    metrics: SubagentMetrics,
    seen_tools: std::collections::HashSet<String>,
}

/// Parse input, resolve the definition/model, construct the child `Thread`,
/// subscribe to its events (streaming progress + bubbling auth + done signal),
/// and start its first turn. All synchronous, on `&mut App`.
fn setup_child(
    cwd: &Arc<PathBuf>,
    depth: u32,
    parent: &WeakEntity<Thread>,
    input: &serde_json::Value,
    ptu: &str,
    cx: &mut App,
) -> Result<SubagentSetup, String> {
    let parsed: AgentToolInput = serde_json::from_value(input.clone())
        .map_err(|e| format!("agent input parse failed: {e}"))?;

    let def_file: Arc<AgentDefinitionFile> = agent_def::global()
        .get(&parsed.subagent_type)
        .cloned()
        .ok_or_else(|| format!("unknown subagent type: {}", parsed.subagent_type))?;
    let def = &def_file.def;

    let child_depth = depth + 1;
    if child_depth > MAX_DEPTH {
        return Err(format!("subagent nesting depth exceeded ({MAX_DEPTH})"));
    }

    let model = resolve_model(&def.model, parent, cx)?;
    // Seed the sub-agent's permission cache with the parent's always-allow
    // grants: a tool the user already "always allows" for the parent should not
    // re-prompt inside the sub-agent. The child cache is still independent, so
    // new grants the user gives inside the sub-agent do not leak back to the
    // parent (they route to the child via the bubbled composite id).
    let parent_snapshot = parent
        .upgrade()
        .map(|p| p.read_with(cx, |t, _| t.permission_snapshot()))
        .unwrap_or_default();
    let permission = Arc::new(PermissionCache::from_snapshot(parent_snapshot));
    // Inherit the parent's approval mode so an AutoReview/Yolo session's
    // sub-agents also bypass the relevant permission gate and (for Yolo) run
    // bash unsandboxed.
    let parent_mode = parent
        .upgrade()
        .map(|p| p.read_with(cx, |t, _| t.approval_mode()))
        .unwrap_or_default();
    let parent_effort = parent
        .upgrade()
        .map(|p| p.read_with(cx, |t, _| t.reasoning_effort()))
        .unwrap_or_default();
    let parent_language = parent
        .upgrade()
        .map(|p| p.read_with(cx, |t, _| t.agent_language()))
        .unwrap_or_default();
    let max_turns = def.max_turns.unwrap_or(10);
    let system_prompt = def_file.system_prompt.clone();

    // Worktree isolation: create a fresh worktree for the child so it works on
    // its own branch with no filesystem overlap with the parent's working tree.
    // The git ops run synchronously via `std::process::Command` — `git worktree
    // add` is sub-second and does not prompt for credentials, so the brief UI
    // thread block is acceptable for the isolation guarantee it buys.
    let isolation = parsed.isolation.as_deref() == Some("worktree");
    let (cwd_path, sandbox, wt_state) = if isolation {
        let project_root = parent
            .upgrade()
            .ok_or_else(|| "parent thread dropped before worktree isolation".to_string())?
            .read_with(cx, |t, _| {
                t.project()
                    .cloned()
                    .unwrap_or_else(|| t.cwd().to_path_buf())
            });
        let (wt_path, branch, git_common_dir) = create_subagent_worktree(&project_root)
            .map_err(|e| format!("subagent worktree creation failed: {e}"))?;
        let sandbox = crate::sandbox::SandboxPolicy::for_worktree(&wt_path, &git_common_dir);
        let state = crate::thread::WorktreeState {
            path: wt_path.clone(),
            prior_cwd: wt_path.clone(),
            branch,
            git_common_dir,
            subagent_created: true,
        };
        (wt_path, sandbox, Some(state))
    } else {
        (
            cwd.as_ref().clone(),
            crate::sandbox::SandboxPolicy::for_project(cwd.as_ref()),
            None,
        )
    };
    let sandbox_for_registry = sandbox.clone();

    let child = Thread::new_subagent(
        cwd_path.clone(),
        model,
        permission,
        parent_mode,
        parent_effort,
        system_prompt,
        max_turns,
        child_depth,
        parsed.subagent_type.clone(),
        parent_language,
        |weak| {
            build_child_registry_with_policy(
                Arc::new(cwd_path.clone()),
                sandbox_for_registry.clone(),
                def,
                child_depth,
                weak,
                def_file.root.clone(),
                parent_language,
            )
        },
        cx,
    );

    // The built-in `Explore` agent skips CLAUDE.md instruction injection to
    // stay fast (Claude Code's Explore/Plan carve-out). A user-authored agent
    // named `Explore` overrides the built-in and keeps instructions.
    if def_file.builtin && def_file.def.name == "Explore" {
        child.update(cx, |c, _cx| c.set_instructions_enabled(false));
    }

    // If the child was spawned into a worktree, record the worktree state on
    // the child so its system prompt advertises it, its sandbox stays
    // worktree-aware, and session-end auto-cleanup can remove it when clean.
    // The registry above was already built with the worktree sandbox, so this
    // does NOT rebuild — it just stamps the state field.
    if let Some(state) = wt_state {
        child.update(cx, |c, cx| {
            c.set_worktree_state(state, cx);
        });
    }

    child.update(cx, |c, cx| {
        c.insert_user_message(parsed.prompt.clone(), cx);
    });

    // Stream the sub-agent's progress to the parent's tool card and bubble
    // authorizations. `Stop` signals completion via the bounded channel.
    // `child_errored` is flipped by the `Error` arm so `run_streaming` can
    // route the result through `Err` → `is_error: true` (a sub-agent that
    // crashed is a failure, not a silent success).
    let (done_tx, done_rx) = async_channel::bounded(1);
    let child_errored = Arc::new(AtomicBool::new(false));
    let child_error_text = Arc::new(std::sync::Mutex::new(None::<String>));
    let metrics = Arc::new(std::sync::Mutex::new(ProgressAccumulator {
        metrics: SubagentMetrics {
            status: Some(ToolCallStatus::Running),
            ..Default::default()
        },
        seen_tools: std::collections::HashSet::new(),
    }));
    let parent_cb = parent.clone();
    let ptu_cb = ptu.to_string();
    // Capture the sub-agent type so bubbled authorization prompts can be
    // prefixed with it — otherwise two parallel sub-agents each running bash
    // produce identical "Tool: bash" overlays the user can't tell apart.
    let subagent_type_cb = def.name.clone();
    let child_weak = child.downgrade();
    let errored_cb = child_errored.clone();
    let error_text_cb = child_error_text.clone();
    let metrics_cb = metrics.clone();
    let sub = cx.subscribe(
        &child,
        move |_child, ev: &ThreadEvent, cx: &mut App| match ev {
            ThreadEvent::Error(e) => {
                errored_cb.store(true, std::sync::atomic::Ordering::Relaxed);
                *error_text_cb.lock().expect("subagent error text poisoned") = Some(e.to_string());
                // A sub-agent error is terminal: unblock the parent so it can
                // collect whatever partial output was produced.
                forward_progress(
                    &parent_cb,
                    &ptu_cb,
                    &subagent_type_cb,
                    &metrics_cb,
                    Some(ToolCallStatus::Error),
                    cx,
                );
                let _ = done_tx.try_send(());
            }
            ThreadEvent::ToolCall {
                id: child_id,
                name,
                title,
                status,
                input,
            } => {
                // The child's `ToolCall` title is already the human-readable
                // action (computed via `tool_title` on the child side); fall
                // back to recomputing it from the structured input only if the
                // child left it empty (historical rebuild path).
                let activity = if !title.is_empty() {
                    title.clone()
                } else {
                    thread::tool_title(
                        name,
                        input.as_ref().unwrap_or(&serde_json::Value::Null),
                        None,
                    )
                };
                let is_new = {
                    let mut acc = metrics_cb.lock().expect("subagent metrics poisoned");
                    let fresh = acc.seen_tools.insert(child_id.clone());
                    if fresh {
                        acc.metrics.record_tool_call(activity.clone());
                    }
                    fresh
                };
                let _ = is_new; // status forwarding happens regardless of novelty
                forward_progress(
                    &parent_cb,
                    &ptu_cb,
                    &subagent_type_cb,
                    &metrics_cb,
                    Some(*status),
                    cx,
                );
            }
            ThreadEvent::TokenUsageUpdated(usage) => {
                {
                    let mut acc = metrics_cb.lock().expect("subagent metrics poisoned");
                    acc.metrics.token_usage = *usage;
                }
                forward_progress(
                    &parent_cb,
                    &ptu_cb,
                    &subagent_type_cb,
                    &metrics_cb,
                    Some(ToolCallStatus::Running),
                    cx,
                );
            }
            ThreadEvent::ToolCallAuthorization {
                id: child_id,
                tool_name,
                summary,
                input,
            } => {
                let composite = format!("{ptu_cb}::{child_id}");
                // Prefix the displayed summary with the sub-agent type so the
                // user can tell which sub-agent a bubbled approval is for.
                // `tool_name` is left untouched: the workspace keys
                // AskUserQuestion rendering off it.
                let prefixed = format!("[{}] {}", subagent_type_cb, summary);
                if let Some(p) = parent_cb.upgrade() {
                    p.update(cx, |t, cx| {
                        t.register_child_auth(
                            composite,
                            child_weak.clone(),
                            child_id.clone(),
                            tool_name.clone(),
                            prefixed,
                            input.clone(),
                            cx,
                        );
                    });
                }
            }
            // `Stop(ToolUse)` is a non-terminal mid-turn signal: the sub-agent
            // finished a stream that requested tools and will run them next.
            // Treating it as done would hand the parent the pre-tool assistant
            // text as the result. Only true terminal stops complete the sub-agent.
            ThreadEvent::Stop(StopReason::EndTurn)
            | ThreadEvent::Stop(StopReason::MaxTokens)
            | ThreadEvent::Stop(StopReason::Refusal) => {
                forward_progress(
                    &parent_cb,
                    &ptu_cb,
                    &subagent_type_cb,
                    &metrics_cb,
                    Some(ToolCallStatus::Success),
                    cx,
                );
                let _ = done_tx.try_send(());
            }
            ThreadEvent::Stop(StopReason::ToolUse) => {}
            _ => {}
        },
    );

    // Emit SubagentStarted so the UI can subscribe to the child thread
    // for the observation panel.
    if let Some(p) = parent.upgrade() {
        p.update(cx, |_t, cx| {
            cx.emit(ThreadEvent::SubagentStarted {
                id: ptu.to_string(),
                subagent_type: parsed.subagent_type.clone(),
                description: parsed.description.clone(),
                child: child.clone(),
            });
        });
    }

    // Start the sub-agent's first turn. Its events drive `done_rx` and the
    // read-only observation panel.
    child.update(cx, |c, cx| {
        c.run_turn(cx);
    });

    Ok(SubagentSetup {
        child,
        done_rx,
        child_errored,
        child_error_text,
        metrics,
        sub,
    })
}

/// Snapshot the accumulator, set the lifecycle `status` (when given), and
/// forward a `SubagentProgress` event to the parent thread's UI subscribers.
/// Holding the metrics lock only long enough to clone + stamp the status keeps
/// the critical section off the gpui notify path.
fn forward_progress(
    parent: &WeakEntity<Thread>,
    ptu: &str,
    subagent_type: &str,
    metrics: &Arc<std::sync::Mutex<ProgressAccumulator>>,
    status: Option<ToolCallStatus>,
    cx: &mut App,
) {
    let Some(p) = parent.upgrade() else {
        return;
    };
    let snapshot = {
        let mut acc = metrics.lock().expect("subagent metrics poisoned");
        if let Some(s) = status {
            acc.metrics.status = Some(s);
        }
        acc.metrics.clone()
    };
    p.update(cx, |_t, cx| {
        cx.emit(ThreadEvent::SubagentProgress {
            id: ptu.to_string(),
            subagent_type: subagent_type.to_string(),
            tool_uses: snapshot.tool_uses,
            token_usage: snapshot.token_usage,
            latest_activity: snapshot.latest_activity,
            status: snapshot.status.unwrap_or(ToolCallStatus::Running),
        });
    });
}

/// Resolve the sub-agent's model: the definition's `model` id if set (resolved
/// via the alias layer so `sonnet`/`opus`/`haiku`/`gpt-5`/`o3` bridge to a live
/// model), else the parent `Thread`'s current model, else the registry's first
/// model.
///
/// When the definition's `model` id is set but cannot be resolved (e.g. a
/// Claude Code plugin frontmatter pins `model: sonnet` but the manox provider
/// config has no Anthropic models), fall back to the parent `Thread`'s model
/// rather than erroring out. This is the Claude Code ecosystem compatibility
/// contract: plugin agents are portable across manox instances with different
/// provider configurations, and an unresolvable model alias should not block
/// spawning — the parent's current model is a reasonable substitute.
fn resolve_model(
    def_model: &Option<String>,
    parent: &WeakEntity<Thread>,
    cx: &mut App,
) -> Result<AnyLanguageModel, String> {
    if let Some(id) = def_model {
        if let Some(m) = crate::model_alias::resolve_model_ref(id) {
            return Ok(m);
        }
        // Unresolvable alias (e.g. `sonnet` with no Anthropic provider
        // configured): fall back to the parent model so a plugin agent can
        // still run. Log so the fallback is visible but never block spawn.
        tracing::info!(
            "sub-agent model alias `{id}` not found in provider config; \
             falling back to parent thread's model"
        );
    }
    if let Ok(Some(m)) = parent.read_with(cx, |t, _| t.model().cloned()) {
        return Ok(m);
    }
    registry::global()
        .models()
        .first()
        .cloned()
        .ok_or_else(|| "no model available".to_string())
}

/// Sentinel returned by `last_assistant_text` when the sub-agent produced no
/// assistant text. `run_streaming` treats it as a failure (returns `Err`) so the
/// parent model sees `is_error: true` rather than a silent non-reply.
const NO_FINAL_SENTINEL: &str = "sub-agent ended without producing a final message";

/// The sub-agent's final result text: the last assistant message's first text
/// block, then a stated "no final message" note. Never returns an empty string
/// so the parent model always receives a non-empty tool result. Does NOT fall
/// back to non-assistant text (e.g. the parent's own prompt or the max-turns
/// summary instruction): echoing those back as the sub-agent's "result" would
/// mislead the parent model into thinking the sub-agent produced a reply.
fn last_assistant_text(msgs: &[Message]) -> String {
    for m in msgs.iter().rev() {
        if m.role != Role::Assistant {
            continue;
        }
        for c in &m.content {
            if let MessageContent::Text(t) = c {
                return t.clone();
            }
        }
    }
    NO_FINAL_SENTINEL.to_string()
}

/// The envelope's `final` for a finished child. A child that emitted a
/// `ThreadEvent::Error` reports the error as its final text — the parent
/// model gets an actionable failure ("sub-agent failed: context overflow…")
/// instead of a bare sentinel or whatever partial text preceded the crash.
fn envelope_final_text(msgs: &[Message], error_text: Option<String>) -> String {
    match error_text {
        Some(e) => format!("sub-agent failed: {e}"),
        None => last_assistant_text(msgs),
    }
}

/// Serialized-byte budget for an envelope's `messages` transcript. The parent
/// model only ever sees `final`; the transcript exists for the UI's
/// expandable panel and reload path, so its size is a persistence/parse
/// concern, not a context one — beyond ~1 MB it is bounded.
const ENVELOPE_MESSAGES_BUDGET: usize = 1024 * 1024;

/// Bound an envelope transcript to [`ENVELOPE_MESSAGES_BUDGET`] serialized
/// bytes: keep the first user message (the delegation prompt) plus the newest
/// messages that fit, inserting a marker after the first when middle messages
/// were omitted. A sub-agent whose tool results flooded its history (the
/// thread-2b1a37c7 failure mode) can no longer persist a 10 MB envelope into
/// the parent thread blob.
fn bound_envelope_messages(msgs: Vec<Message>) -> Vec<Message> {
    let size_of = |m: &Message| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0);
    let total: usize = msgs.iter().map(size_of).sum();
    if total <= ENVELOPE_MESSAGES_BUDGET {
        return msgs;
    }
    let mut out: Vec<Message> = Vec::new();
    let Some(first) = msgs.first() else {
        return msgs;
    };
    let mut used = size_of(first);
    out.push(first.clone());
    // Reserve headroom for the omission marker so the tail still fits.
    let mut tail: Vec<Message> = Vec::new();
    for m in msgs.iter().skip(1).rev() {
        let len = size_of(m);
        if used + len > ENVELOPE_MESSAGES_BUDGET {
            break;
        }
        used += len;
        tail.push(m.clone());
    }
    tail.reverse();
    let omitted = msgs.len() - out.len() - tail.len();
    if omitted > 0 {
        out.push(Message::user(format!(
            "[{omitted} earlier sub-agent messages omitted]"
        )));
    }
    out.extend(tail);
    out
}

/// Persisted `agent` tool-result envelope: the model-facing final text plus the
/// full sub-agent conversation, so the observation panel survives a reload.
/// The envelope is the canonical ToolResult content persisted to the DB;
/// `build_completion_request` strips it to `final` before the request reaches
/// the model, so the sub-conversation never leaks into the parent's context.
/// `metrics` keeps backend telemetry and the terminal status available for
/// snapshot restoration, but is not rendered in the main message row.
#[derive(Deserialize)]
pub(crate) struct AgentToolResultPayload {
    #[serde(rename = "final")]
    final_text: String,
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(default)]
    metrics: Option<SubagentMetrics>,
}

/// Aggregated telemetry for one `agent` tool invocation, emitted live through
/// `ThreadEvent::SubagentProgress` and persisted in the result envelope. It
/// never enters the model's message history or the compact main-message UI;
/// the parent model still only sees the `final` text. `status` is `None` until
/// the sub-agent's lifecycle pins it.
///
/// Modeled on the `AgentProgress` shape (toolCount / tokens / recentTools /
/// currentTool) used by the pi/oh-my-pi coding-agent, kept lean: the
/// activity-summary text the cockpit renders is derived from the same shared
/// summarizer the `ThinkingContainer` uses, so only the aggregate counts + a
/// single latest-activity line need to travel here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubagentMetrics {
    pub tool_uses: u32,
    pub token_usage: TokenUsage,
    pub latest_activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
}

impl SubagentMetrics {
    /// A new child tool call started. `tool_uses` counts distinct child tool ids,
    /// not status-change events (one tool emits several `ToolCall` events as its
    /// status transitions), so the caller must gate on id novelty.
    fn record_tool_call(&mut self, title: String) {
        self.tool_uses = self.tool_uses.saturating_add(1);
        self.latest_activity = Some(title);
    }
}

/// The model-facing final text from an `agent` tool result. Parses the JSON
/// envelope when present; falls back to the raw content for legacy or non-json
/// results. Used by `Thread::build_completion_request` to strip the envelope so
/// only the final text reaches the parent model.
pub fn agent_final_text(content: &str) -> String {
    serde_json::from_str::<AgentToolResultPayload>(content)
        .map(|p| p.final_text)
        .unwrap_or_else(|_| content.to_string())
}

/// The persisted sub-agent conversation, when the content is the JSON envelope.
/// Used by the UI to rebuild the read-only observation panel after a reload.
pub fn agent_sub_messages(content: &str) -> Option<Vec<Message>> {
    serde_json::from_str::<AgentToolResultPayload>(content)
        .ok()
        .map(|p| p.messages)
}

/// The persisted sub-agent telemetry, when the content is the JSON envelope.
/// `None` on legacy envelopes written before `metrics` existed.
pub fn agent_metrics(content: &str) -> Option<SubagentMetrics> {
    serde_json::from_str::<AgentToolResultPayload>(content)
        .ok()
        .and_then(|p| p.metrics)
}

/// Whether `name` passes the definition's `tools`/`disallowed_tools` filters.
/// `disallowed_tools` takes precedence; an absent `tools` whitelist inherits all.
fn is_tool_allowed(name: &str, def: &AgentDefinition) -> bool {
    if let Some(d) = &def.disallowed_tools
        && d.as_vec().iter().any(|x| x == name)
    {
        return false;
    }
    if let Some(a) = &def.tools {
        return a.as_vec().iter().any(|x| x == name);
    }
    true
}

/// Whether the sub-agent may itself spawn sub-agents: its definition opts in
/// and the depth cap has not been reached.
fn can_nest(def: &AgentDefinition, child_depth: u32) -> bool {
    def.allow_nesting && child_depth < MAX_DEPTH
}

/// Same as [`build_child_registry`] but with an explicit sandbox policy. The
/// worktree-isolation path passes a `for_worktree` policy so the child's write
/// confinement anchors on its own worktree (the parent's project root is out
/// of reach) while the bound repo's `.git` and network stay open for git ops.
fn build_child_registry_with_policy(
    cwd: Arc<PathBuf>,
    sandbox: crate::sandbox::SandboxPolicy,
    def: &AgentDefinition,
    child_depth: u32,
    child_weak: WeakEntity<Thread>,
    plugin_root: Option<PathBuf>,
    lang: crate::language::Language,
) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    for tool in super::base_tools_with_policy(cwd.clone(), sandbox, plugin_root, lang) {
        if is_tool_allowed(tool.name(), def) {
            reg.register(tool);
        }
    }
    // self_info is per-thread (main-thread-only in `main_registry`); sub-agents
    // register it here subject to the same allow/disallow filter. It is
    // stateless now (reads the per-call `ToolContext`), but stays main+child
    // only — never in `base_tools`.
    if is_tool_allowed(super::SELF_INFO, def) {
        reg.register(super::self_info::new());
    }
    if can_nest(def, child_depth) {
        reg.register(Arc::new(SpawnAgentTool::new(
            cwd.clone(),
            child_depth,
            child_weak,
            lang,
        )) as AnyAgentTool);
    }
    reg
}

/// Spec for spawning a long-lived team worker member. `TeamCreate` /
/// `TeamSpawn` tools build these from their input; [`spawn_team_member`] does
/// the actual `Thread` construction.
pub(crate) struct MemberSpec {
    pub name: String,
    pub subagent_type: String,
    pub prompt: String,
}

/// Spawn a long-lived team worker: an independent `Entity<Thread>` at depth 1
/// sharing the leader's cwd, inheriting the leader's model / approval mode /
/// reasoning effort / always-allow permission grants, with the sub-agent
/// definition's tool allowlist PLUS the shared team coordination tools
/// (`Task*`, `SendMessage`). The member's `team` back-reference is set so
/// those tools reach the shared [`crate::team::TaskList`] and the message
/// router.
///
/// Unlike a one-shot `agent` sub-agent, a member is fire-and-forget: it runs to
/// self-completion or `max_turns`, reporting back via `SendMessage`. Its
/// `AgentText`/`AgentThinking` are NOT streamed to the leader — the member
/// panel subscribes to the member `Thread` directly. `ToolCallAuthorization`
/// bubbles to the leader as `<name>::<auth>` (reusing the composite-id route);
/// a terminal `Stop` flushes the member's queued inbox. The returned
/// [`gpui::Subscription`] keeps those routes alive for the member's tenure.
pub(crate) fn spawn_team_member(
    leader: &WeakEntity<Thread>,
    team: gpui::Entity<crate::team::Team>,
    spec: MemberSpec,
    cx: &mut App,
) -> Result<(gpui::Entity<Thread>, gpui::Subscription), String> {
    let def_file: Arc<AgentDefinitionFile> = agent_def::global()
        .get(&spec.subagent_type)
        .cloned()
        .ok_or_else(|| format!("unknown subagent type: {}", spec.subagent_type))?;
    let def = &def_file.def;

    let leader_ent = leader
        .upgrade()
        .ok_or_else(|| "leader thread dropped during team_spawn".to_string())?;

    let cwd = leader_ent.read_with(cx, |t, _| t.cwd().to_path_buf());
    let model = resolve_model(&def.model, leader, cx)?;
    let parent_snapshot = leader_ent.read_with(cx, |t, _| t.permission_snapshot());
    let permission = Arc::new(PermissionCache::from_snapshot(parent_snapshot));
    let parent_mode = leader_ent.read_with(cx, |t, _| t.approval_mode());
    let parent_effort = leader_ent.read_with(cx, |t, _| t.reasoning_effort());
    let parent_language = leader_ent.read_with(cx, |t, _| t.agent_language());
    let max_turns = def.max_turns.unwrap_or(10);
    let system_prompt = def_file.system_prompt.clone();
    let depth = 1u32;
    let sandbox = crate::sandbox::SandboxPolicy::for_project(&cwd);
    let sandbox_for_registry = sandbox.clone();

    let member = Thread::new_subagent(
        cwd.clone(),
        model,
        permission,
        parent_mode,
        parent_effort,
        system_prompt,
        max_turns,
        depth,
        spec.name.clone(),
        parent_language,
        |weak| {
            let mut reg = build_child_registry_with_policy(
                Arc::new(cwd.clone()),
                sandbox_for_registry.clone(),
                def,
                depth,
                weak,
                def_file.root.clone(),
                parent_language,
            );
            for tool in crate::team::tools::shared_tools() {
                reg.register(tool);
            }
            reg
        },
        cx,
    );

    // Same Explore carve-out as the `agent` tool path: the built-in Explore
    // definition skips CLAUDE.md instructions; overrides keep them.
    if def_file.builtin && def_file.def.name == "Explore" {
        member.update(cx, |t, _cx| t.set_instructions_enabled(false));
    }
    // Attach the team so the member's Task*/SendMessage reach the shared
    // list + router. This is the member→team strong edge; `Team::disband`
    // clears it before dropping the roster.
    member.update(cx, |t, cx| t.set_team(team.clone(), cx));
    member.update(cx, |t, cx| {
        t.insert_user_message(spec.prompt.clone(), cx);
    });

    let leader_cb = leader.clone();
    let team_weak = team.downgrade();
    let name_cb = spec.name.clone();
    let sub = cx.subscribe(
        &member,
        move |_member, ev: &ThreadEvent, cx: &mut App| match ev {
            ThreadEvent::ToolCallAuthorization {
                id: child_id,
                tool_name,
                summary,
                input,
            } => {
                let composite = format!("{name_cb}::{child_id}");
                let prefixed = format!("[{name_cb}] {summary}");
                if let Some(p) = leader_cb.upgrade() {
                    p.update(cx, |t, cx| {
                        t.register_child_auth(
                            composite,
                            _member.downgrade(),
                            child_id.clone(),
                            tool_name.clone(),
                            prefixed,
                            input.clone(),
                            cx,
                        );
                    });
                }
            }
            ThreadEvent::Stop(StopReason::EndTurn)
            | ThreadEvent::Stop(StopReason::MaxTokens)
            | ThreadEvent::Stop(StopReason::Refusal) => {
                let tw = team_weak.clone();
                let n = name_cb.clone();
                cx.defer(move |cx| {
                    if let Some(t) = tw.upgrade() {
                        t.update(cx, |tm, cx| tm.flush_inbox(&n, cx));
                    }
                });
            }
            ThreadEvent::Stop(StopReason::ToolUse) => {}
            _ => {}
        },
    );

    member.update(cx, |t, cx| {
        t.run_turn(cx);
    });

    Ok((member, sub))
}

/// A short capability tag for a sub-agent definition, derived from its
/// `tools`/`disallowed_tools`: `read-only` when it can neither write files nor
/// run bash, otherwise the union of `write`/`bash`. Advertised in the tool
/// description so the parent model does not delegate write/exec work to a
/// read-only sub-agent — the failure mode behind thread 6cd3d096, where three
/// `plan` sub-agents were asked to write files they could not touch and each
/// replied "I'm in read-only mode".
fn capability_tag(def: &AgentDefinition) -> &'static str {
    let can_write = is_tool_allowed(super::WRITE, def) || is_tool_allowed(super::EDIT, def);
    let can_bash = is_tool_allowed(super::BASH, def);
    match (can_write, can_bash) {
        (true, true) => "write+bash",
        (true, false) => "write",
        (false, true) => "bash",
        (false, false) => "read-only",
    }
}

/// Create a fresh git worktree for a sub-agent and return `(worktree_path,
/// branch, git_common_dir)`. Synchronous (`std::process::Command`) because
/// `setup_child` runs on the UI thread with `&mut App`; `git worktree add` is
/// sub-second and never prompts for credentials, so the brief block is the
/// trade-off for not restructuring the sync spawn handshake. The worktree
/// lives under `<project_root>/.claude/worktrees/subagent-<short>` on a branch
/// of the same name, based off `origin/<default-branch>` (fallback `HEAD`).
fn create_subagent_worktree(project_root: &Path) -> Result<(PathBuf, String, PathBuf), String> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let short = &id[..8];
    let branch = format!("subagent-{short}");
    let wt_path = project_root.join(".claude").join("worktrees").join(&branch);
    if let Some(parent) = wt_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create_dir_all: {e}"))?;
    }
    let base_ref = resolve_base_ref_sync(project_root);
    let path_str = wt_path.display().to_string();
    let mut args: Vec<&str> = vec!["worktree", "add", "-b", &branch, &path_str];
    let owned_base;
    if base_ref != "HEAD" {
        owned_base = base_ref.clone();
        args.push(&owned_base);
    } else {
        args.push("HEAD");
    }
    run_git_sync(project_root, &args).map_err(|e| format!("git worktree add: {e}"))?;
    let git_dir_str = run_git_sync(&wt_path, &["rev-parse", "--git-common-dir"])
        .map_err(|e| format!("git rev-parse --git-common-dir: {e}"))?;
    let git_common_dir = absolutize_path(&wt_path, &git_dir_str);
    Ok((wt_path, branch, git_common_dir))
}

/// Synchronous `git` invocation returning trimmed stdout. Used only by
/// [`create_subagent_worktree`] on the UI thread; the main-thread worktree
/// tools use the async tokio path in `tools/worktree.rs`.
fn run_git_sync(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `origin/<default-branch>` when a remote HEAD is configured, else `HEAD`.
fn resolve_base_ref_sync(project_root: &Path) -> String {
    match run_git_sync(project_root, &["rev-parse", "--abbrev-ref", "origin/HEAD"]) {
        Ok(s) => {
            let s = s.trim();
            if s.is_empty() || s == "origin/HEAD" {
                "HEAD".into()
            } else {
                s.to_string()
            }
        }
        Err(_) => "HEAD".into(),
    }
}

/// Resolve a possibly-relative `git rev-parse --git-common-dir` result against
/// the worktree dir so the sandbox de-protects the right path.
fn absolutize_path(worktree_dir: &Path, git_common_dir: &str) -> PathBuf {
    let p = PathBuf::from(git_common_dir);
    if p.is_absolute() {
        p
    } else {
        worktree_dir.join(p)
    }
}

/// Compose the tool description advertised to the parent model, listing the
/// available sub-agent types with a capability tag and their one-line
/// descriptions.
fn build_description(lang: crate::language::Language) -> Arc<str> {
    let subagents: Vec<crate::prompt::SubagentTypeData> = agent_def::global()
        .entries()
        .into_iter()
        .map(|(key, d)| crate::prompt::SubagentTypeData {
            name: key.clone(),
            capability: capability_tag(&d.def),
            description: d.def.description.clone(),
        })
        .collect();
    let s = crate::prompt::render(
        crate::prompt::PromptTemplate::AgentToolDescription,
        lang,
        &crate::prompt::AgentToolDescriptionData { subagents },
    )
    .expect("agent tool description render");
    Arc::<str>::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_def::AgentDefinition;

    fn def(
        tools: Option<Vec<String>>,
        disallowed: Option<Vec<String>>,
        nesting: bool,
    ) -> AgentDefinition {
        AgentDefinition {
            name: "test".to_string(),
            description: "test".to_string(),
            tools: tools.map(crate::agent_def::ToolsList::List),
            disallowed_tools: disallowed.map(crate::agent_def::ToolsList::List),
            model: None,
            max_turns: None,
            allow_nesting: nesting,
        }
    }

    #[test]
    fn whitelist_keeps_only_listed_tools() {
        let d = def(
            Some(vec!["Read".to_string(), "Grep".to_string()]),
            None,
            false,
        );
        assert!(is_tool_allowed("Read", &d));
        assert!(is_tool_allowed("Grep", &d));
        assert!(!is_tool_allowed("Write", &d));
        assert!(!is_tool_allowed("Bash", &d));
    }

    #[test]
    fn blacklist_removes_tools() {
        let d = def(
            None,
            Some(vec!["Bash".to_string(), "Write".to_string()]),
            false,
        );
        assert!(!is_tool_allowed("Bash", &d));
        assert!(!is_tool_allowed("Write", &d));
        assert!(is_tool_allowed("Read", &d));
    }

    #[test]
    fn no_filters_inherits_all() {
        let d = def(None, None, false);
        assert!(is_tool_allowed("Read", &d));
        assert!(is_tool_allowed("Bash", &d));
        assert!(is_tool_allowed("Agent", &d));
    }

    #[test]
    fn blacklist_wins_over_whitelist() {
        // When both are set, blacklist takes precedence over the whitelist.
        let d = def(
            Some(vec!["Read".to_string(), "Bash".to_string()]),
            Some(vec!["Bash".to_string()]),
            false,
        );
        assert!(is_tool_allowed("Read", &d));
        assert!(!is_tool_allowed("Bash", &d));
        assert!(!is_tool_allowed("Grep", &d));
    }

    #[test]
    fn can_nest_respects_allow_flag_and_depth() {
        let on = def(None, None, true);
        let off = def(None, None, false);
        assert!(can_nest(&on, 1));
        assert!(can_nest(&on, MAX_DEPTH - 1));
        assert!(!can_nest(&on, MAX_DEPTH));
        assert!(!can_nest(&off, 1));
    }

    #[test]
    fn input_schema_requires_short_description() {
        let schema = super::super::schema::<AgentToolInput>();
        let required = schema["required"]
            .as_array()
            .expect("agent schema required fields");
        assert!(required.iter().any(|field| field == "description"));
        assert!(required.iter().any(|field| field == "subagent_type"));
        assert!(required.iter().any(|field| field == "prompt"));
    }

    #[test]
    fn capability_tag_read_only_when_write_and_bash_disallowed() {
        // The bundled `plan`/`explore` profile: explicit read-only allowlist,
        // write/exec tools disallowed. Must advertise `read-only` so the parent
        // model does not delegate write work here (thread 6cd3d096).
        let d = def(
            Some(vec!["Read".to_string(), "Grep".to_string()]),
            Some(vec![
                "Write".to_string(),
                "Edit".to_string(),
                "Bash".to_string(),
            ]),
            false,
        );
        assert_eq!(capability_tag(&d), "read-only");
    }

    #[test]
    fn capability_tag_reflects_write_and_bash_availability() {
        let write_only = def(Some(vec!["Write".to_string()]), None, false);
        assert_eq!(capability_tag(&write_only), "write");
        let bash_only = def(Some(vec!["Bash".to_string()]), None, false);
        assert_eq!(capability_tag(&bash_only), "bash");
        let both = def(
            Some(vec!["Write".to_string(), "Bash".to_string()]),
            None,
            false,
        );
        assert_eq!(capability_tag(&both), "write+bash");
    }

    #[test]
    fn extracts_last_assistant_text() {
        // The trailing assistant message's first text block is the result; an
        // earlier text block in the same message is ignored in favor of the
        // last message overall.
        let msgs = vec![
            Message::user("hi".to_string()),
            Message::assistant(vec![MessageContent::Text("first".to_string())]),
        ];
        assert_eq!(last_assistant_text(&msgs), "first");
    }

    #[test]
    fn no_assistant_yields_sentinel_not_prompt() {
        // With no assistant message, the sub-agent produced no reply. Return
        // the honest sentinel rather than echoing the parent's own prompt (or
        // the max-turns summary instruction) back as the "result" — that would
        // mislead the parent model into thinking the sub-agent answered.
        let msgs = vec![Message::user("hi".to_string())];
        assert_eq!(last_assistant_text(&msgs), NO_FINAL_SENTINEL);
        // Truly no text anywhere → same sentinel, never an empty string.
        let msgs: Vec<Message> = vec![Message::user_with_content(vec![])];
        assert_eq!(last_assistant_text(&msgs), NO_FINAL_SENTINEL);
    }

    #[test]
    fn envelope_final_text_reports_child_error() {
        let msgs = vec![
            Message::user("do work".to_string()),
            Message::assistant(vec![MessageContent::Text("partial".to_string())]),
        ];
        assert_eq!(
            envelope_final_text(&msgs, Some("context overflow".to_string())),
            "sub-agent failed: context overflow"
        );
        assert_eq!(envelope_final_text(&msgs, None), "partial");
    }

    #[test]
    fn envelope_messages_under_budget_pass_through() {
        let msgs = vec![
            Message::user("task".to_string()),
            Message::assistant(vec![MessageContent::Text("done".to_string())]),
        ];
        let bound = bound_envelope_messages(msgs.clone());
        assert_eq!(bound.len(), msgs.len());
        assert!(!bound.iter().any(|m| matches!(
            m.content.first(),
            Some(MessageContent::Text(t)) if t.contains("omitted")
        )));
    }

    #[test]
    fn envelope_messages_over_budget_keep_head_and_tail() {
        // A flood of oversized tool-result-style messages in the middle must
        // collapse: first message (the prompt) + marker + fitting tail.
        let blob = "y".repeat(ENVELOPE_MESSAGES_BUDGET / 4);
        let mut msgs = vec![Message::user("the task".to_string())];
        for i in 0..8 {
            msgs.push(Message::user(format!("flood-{i}-{blob}")));
        }
        msgs.push(Message::assistant(vec![MessageContent::Text(
            "final".to_string(),
        )]));
        let bound = bound_envelope_messages(msgs);
        let first_text = match bound.first().and_then(|m| m.content.first()) {
            Some(MessageContent::Text(t)) => t.clone(),
            _ => panic!("first message must be the prompt"),
        };
        assert_eq!(first_text, "the task");
        assert!(
            bound.iter().any(|m| matches!(
                m.content.first(),
                Some(MessageContent::Text(t)) if t.contains("earlier sub-agent messages omitted")
            )),
            "omission marker present: {:?}",
            bound.len()
        );
        let last = bound.last().expect("tail preserved");
        assert!(matches!(
            last.content.first(),
            Some(MessageContent::Text(t)) if t == "final"
        ));
        let serialized: usize = bound
            .iter()
            .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
            .sum();
        assert!(
            serialized <= ENVELOPE_MESSAGES_BUDGET + 2048,
            "bounded to ~budget, got {serialized}"
        );
    }

    #[test]
    fn metrics_round_trip_preserves_counts() {
        let m = SubagentMetrics {
            tool_uses: 28,
            token_usage: TokenUsage {
                input_tokens: 12000,
                output_tokens: 5300,
                ..Default::default()
            },
            latest_activity: Some("Read src/lib.rs".to_string()),
            status: Some(ToolCallStatus::Success),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: SubagentMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tool_uses, 28);
        assert_eq!(back.token_usage.input_tokens, 12000);
        assert_eq!(back.token_usage.output_tokens, 5300);
        assert_eq!(back.latest_activity.as_deref(), Some("Read src/lib.rs"));
        assert_eq!(back.status, Some(ToolCallStatus::Success));
    }

    #[test]
    fn envelope_carries_metrics_for_new_envelopes() {
        let envelope = serde_json::json!({
            "final": "done",
            "messages": <Vec<Message>>::new(),
            "metrics": SubagentMetrics {
                tool_uses: 3,
                token_usage: TokenUsage { input_tokens: 100, ..Default::default() },
                latest_activity: Some("grep foo".to_string()),
                status: Some(ToolCallStatus::Success),
            }
        })
        .to_string();
        let m = agent_metrics(&envelope).expect("metrics should parse");
        assert_eq!(m.tool_uses, 3);
        assert_eq!(m.token_usage.input_tokens, 100);
        assert_eq!(m.latest_activity.as_deref(), Some("grep foo"));
        assert_eq!(m.status, Some(ToolCallStatus::Success));
        // The model-facing final text still parses from the same envelope.
        assert_eq!(agent_final_text(&envelope), "done");
    }

    #[test]
    fn envelope_without_metrics_parses_as_none() {
        // Legacy envelopes written before telemetry existed have no `metrics`
        // key. The UI treats that as empty counters, never a parse failure.
        let legacy = serde_json::json!({
            "final": "old",
            "messages": <Vec<Message>>::new()
        })
        .to_string();
        assert!(agent_metrics(&legacy).is_none());
        assert_eq!(agent_final_text(&legacy), "old");
    }
}
