//! The `agent` tool: spawn a sub-agent with its own context, restricted tools,
//! and system prompt, then return its final assistant message as the tool
//! result. Mirrors Claude Code's `Agent` tool and Codex's `spawn_agent`.
//!
//! Design:
//! - Each sub-agent is an independent `Entity<Thread>` with a fresh message
//!   list, a restricted `ToolRegistry`, and an independent `PermissionCache`.
//! - The parent-side `tool_use_id` (from `ToolOutputSink::tool_call_id`) is the
//!   stable key for snapshot storage and for composing bubbled-auth ids.
//! - The sub-agent's `AgentText`/`AgentThinking`/`Error` events stream back to
//!   the parent's tool card via the sink; `ToolCallAuthorization` is re-emitted
//!   on the parent under a composite id `<parent_tool_use_id>::<child_id>` so
//!   the existing approval overlay resolves it transparently, and the decision
//!   is routed back to the child.
//! - Nesting depth is capped at `MAX_DEPTH`; a sub-agent may only spawn its own
//!   sub-agents when its definition sets `allow_nesting: true`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gpui::{App, AppContext, AsyncApp, Task, WeakEntity};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::agent_def::{self, AgentDefinition, AgentDefinitionFile};
use crate::language_model::{AnyLanguageModel, MessageContent, Role, StopReason};
use crate::message::Message;
use crate::provider::registry;
use crate::thread::{Thread, ThreadEvent};
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
    pub fn new(cwd: Arc<PathBuf>, depth: u32, parent: WeakEntity<Thread>) -> Self {
        let desc = build_description();
        Self {
            cwd,
            depth,
            parent,
            desc,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct AgentToolInput {
    /// Name of the sub-agent definition to spawn (from `~/.config/cx/manox/agents/*.md`).
    subagent_type: String,
    /// The task to delegate. Becomes the sub-agent's first user message; the
    /// sub-agent has no access to the parent's conversation history, so include
    /// any needed file paths, error text, or context here.
    prompt: String,
}

impl AgentToolTrait for SpawnAgentTool {
    fn name(&self) -> &str {
        "agent"
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
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        // The non-streaming entry point delegates to `run_streaming` with a
        // discard sink. In practice the owning `Thread` always calls
        // `run_streaming` directly, so this path is rarely hit.
        let (sink, _rx) = ToolOutputSink::channel(Arc::from(""));
        let _ = _rx;
        self.run_streaming(input, cancel, sink, cx)
    }

    fn run_streaming(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        sink: ToolOutputSink,
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
        let setup = match setup_child(&cwd, depth, &parent, &input, &sink, &ptu, cx) {
            Ok(s) => s,
            Err(e) => return cx.background_spawn(async move { Err(e) }),
        };

        let child = setup.child;
        let done_rx = setup.done_rx;
        let child_errored = setup.child_errored;
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

            // Capture the sub-agent's full conversation and extract its final
            // assistant text as the tool result.
            let msgs = child.read_with(cx, |c, _| c.messages().to_vec());
            let final_text = last_assistant_text(&msgs);
            let errored = child_errored.load(std::sync::atomic::Ordering::Relaxed)
                || final_text == NO_FINAL_SENTINEL;
            drop(sub);

            // The tool result is a JSON envelope {"final":..., "messages":[...]}.
            // The envelope in the parent's ToolResult is the single source of
            // truth: build_completion_request strips it to `final` for the model
            // (context isolation), and the UI parses `messages` from it for the
            // expandable panel — both live and after reload. No separate in-memory
            // snapshot map is kept, so there is nothing to leak across a long
            // session with many sub-agent calls.
            //
            // Returning the envelope as `Err` (rather than `Ok`) when the
            // sub-agent errored or produced no final message routes through
            // `Thread::run_tool_inner`'s `Err(e) => (e, true)` arm — the ToolResult
            // keeps the envelope as its content (so the UI panel and the
            // `agent_final_text`/`agent_sub_messages` parsers still work) but is
            // flagged `is_error: true`, so the parent model sees the failure
            // instead of mistaking a non-reply for success (thread 6cd3d096).
            let payload = serde_json::json!({ "final": final_text, "messages": msgs });
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
/// the sub-agent emits `ThreadEvent::Error`.
struct SubagentSetup {
    child: gpui::Entity<Thread>,
    done_rx: async_channel::Receiver<()>,
    child_errored: Arc<AtomicBool>,
    sub: gpui::Subscription,
}

/// Parse input, resolve the definition/model, construct the child `Thread`,
/// subscribe to its events (streaming progress + bubbling auth + done signal),
/// and start its first turn. All synchronous, on `&mut App`.
fn setup_child(
    cwd: &Arc<PathBuf>,
    depth: u32,
    parent: &WeakEntity<Thread>,
    input: &serde_json::Value,
    sink: &ToolOutputSink,
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
    // Inherit the parent's YOLO flag so a YOLO session's sub-agents also skip
    // approvals and run bash unsandboxed.
    let parent_yolo = parent
        .upgrade()
        .map(|p| p.read_with(cx, |t, _| t.yolo()))
        .unwrap_or(false);
    let max_turns = def.max_turns.unwrap_or(10);
    let system_prompt = def_file.system_prompt.clone();
    let cwd_path = cwd.as_ref().clone();

    let child = Thread::new_subagent(
        cwd_path,
        model,
        permission,
        parent_yolo,
        system_prompt,
        max_turns,
        child_depth,
        |weak| build_child_registry(cwd, def, child_depth, weak),
        cx,
    );

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
    let sink_cb = sink.clone();
    let parent_cb = parent.clone();
    let ptu_cb = ptu.to_string();
    // Capture the sub-agent type so bubbled authorization prompts can be
    // prefixed with it — otherwise two parallel sub-agents each running bash
    // produce identical "工具：bash" overlays the user can't tell apart.
    let subagent_type_cb = def.name.clone();
    let child_weak = child.downgrade();
    let errored_cb = child_errored.clone();
    let sub = cx.subscribe(
        &child,
        move |_child, ev: &ThreadEvent, cx: &mut App| match ev {
            ThreadEvent::AgentText(t) | ThreadEvent::AgentThinking(t) => {
                sink_cb.try_emit(t);
            }
            ThreadEvent::Error(e) => {
                errored_cb.store(true, std::sync::atomic::Ordering::Relaxed);
                sink_cb.try_emit(&e.to_string());
                // A sub-agent error is terminal: unblock the parent so it can
                // collect whatever partial output was produced.
                let _ = done_tx.try_send(());
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
                let _ = done_tx.try_send(());
            }
            ThreadEvent::Stop(StopReason::ToolUse) => {}
            _ => {}
        },
    );

    // Start the sub-agent's first turn. Its events drive `done_rx`/the sink.
    child.update(cx, |c, cx| {
        c.run_turn(cx);
    });

    Ok(SubagentSetup {
        child,
        done_rx,
        child_errored,
        sub,
    })
}

/// Resolve the sub-agent's model: the definition's `model` id if set (resolved
/// via the alias layer so `sonnet`/`opus`/`haiku`/`gpt-5`/`o3` bridge to a live
/// model), else the parent `Thread`'s current model, else the registry's first
/// model.
fn resolve_model(
    def_model: &Option<String>,
    parent: &WeakEntity<Thread>,
    cx: &mut App,
) -> Result<AnyLanguageModel, String> {
    if let Some(id) = def_model {
        if let Some(m) = crate::model_alias::resolve_model_ref(id) {
            return Ok(m);
        }
        return Err(format!("sub-agent model not found: {id}"));
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

/// Persisted `agent` tool-result envelope: the model-facing final text plus the
/// full sub-agent conversation, so the expandable panel survives a reload. The
/// envelope is the canonical ToolResult content (persisted to DB, used by the UI
/// to rebuild `sub_messages`); `build_completion_request` strips it to `final`
/// before the request reaches the model, so the sub-conversation never leaks
/// into the parent's context.
#[derive(Deserialize)]
pub(crate) struct AgentToolResultPayload {
    #[serde(rename = "final")]
    final_text: String,
    #[serde(default)]
    messages: Vec<Message>,
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
/// Used by the UI to rebuild the expandable panel after a reload (the in-memory
/// snapshot map is empty on restart, so the envelope is the only source).
pub fn agent_sub_messages(content: &str) -> Option<Vec<Message>> {
    serde_json::from_str::<AgentToolResultPayload>(content)
        .ok()
        .map(|p| p.messages)
}

/// Whether `name` passes the definition's `tools`/`disallowed_tools` filters.
/// `disallowed_tools` takes precedence; an absent `tools` whitelist inherits all.
fn is_tool_allowed(name: &str, def: &AgentDefinition) -> bool {
    if let Some(d) = &def.disallowed_tools
        && d.iter().any(|x| x == name)
    {
        return false;
    }
    if let Some(a) = &def.tools {
        return a.iter().any(|x| x == name);
    }
    true
}

/// Whether the sub-agent may itself spawn sub-agents: its definition opts in
/// and the depth cap has not been reached.
fn can_nest(def: &AgentDefinition, child_depth: u32) -> bool {
    def.allow_nesting && child_depth < MAX_DEPTH
}

/// Build the sub-agent's restricted tool registry from its definition: the
/// built-in tools filtered by `tools`/`disallowed_tools`, plus the `agent`
/// tool itself only when `allow_nesting` is set and depth permits.
fn build_child_registry(
    cwd: &Arc<PathBuf>,
    def: &AgentDefinition,
    child_depth: u32,
    child_weak: WeakEntity<Thread>,
) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    for tool in super::base_tools(cwd.clone(), child_weak.clone()) {
        if is_tool_allowed(tool.name(), def) {
            reg.register(tool);
        }
    }
    // self_info is per-thread (needs the child's WeakEntity), so it can't live
    // in base_tools; register it here, subject to the same allow/disallow filter.
    if is_tool_allowed("self_info", def) {
        reg.register(super::self_info::new(child_weak.clone()));
    }
    if can_nest(def, child_depth) {
        reg.register(
            Arc::new(SpawnAgentTool::new(cwd.clone(), child_depth, child_weak)) as AnyAgentTool,
        );
    }
    reg
}

/// A short capability tag for a sub-agent definition, derived from its
/// `tools`/`disallowed_tools`: `read-only` when it can neither write files nor
/// run bash, otherwise the union of `write`/`bash`. Advertised in the tool
/// description so the parent model does not delegate write/exec work to a
/// read-only sub-agent — the failure mode behind thread 6cd3d096, where three
/// `plan` sub-agents were asked to write files they could not touch and each
/// replied "I'm in read-only mode".
fn capability_tag(def: &AgentDefinition) -> &'static str {
    let can_write = is_tool_allowed("write_file", def) || is_tool_allowed("edit_file", def);
    let can_bash = is_tool_allowed("bash", def);
    match (can_write, can_bash) {
        (true, true) => "write+bash",
        (true, false) => "write",
        (false, true) => "bash",
        (false, false) => "read-only",
    }
}

/// Compose the tool description advertised to the parent model, listing the
/// available sub-agent types with a capability tag and their one-line
/// descriptions.
fn build_description() -> Arc<str> {
    let mut s = String::from(
        "Spawn a sub-agent to handle a focused subtask. The sub-agent runs in \
         its own fresh context (no parent history), with a restricted tool set \
         and a specialized system prompt. Only its final assistant message \
         returns as the tool result. Useful for: exploring code, research, \
         parallel subtasks, or any work that would bloat the main context.",
    );
    let defs = agent_def::global().list();
    if !defs.is_empty() {
        s.push_str("\n\nAvailable subagent_type values:");
        for d in defs {
            s.push_str(&format!(
                "\n- {} ({}): {}",
                d.def.name,
                capability_tag(&d.def),
                d.def.description
            ));
        }
        s.push_str(
            "\n\nThe capability tag in parentheses shows what each sub-agent can \
             do: `read-only` sub-agents cannot write files or run bash — do not \
             delegate write/exec work to them (they will refuse and waste a \
             round). Each sub-agent starts from a blank context with no parent \
             history, so pin any interface contract the sub-agent must honor \
             (exact function names, signatures, types) directly in the prompt.",
        );
    } else {
        s.push_str(
            "\n\nNo sub-agent definitions are loaded. Add Markdown files under \
             ~/.config/cx/manox/agents/ (frontmatter name/description/tools/model \
             + body as system prompt) and restart.",
        );
    }
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
            tools,
            disallowed_tools: disallowed,
            model: None,
            max_turns: None,
            allow_nesting: nesting,
        }
    }

    #[test]
    fn whitelist_keeps_only_listed_tools() {
        let d = def(
            Some(vec!["read_file".to_string(), "grep".to_string()]),
            None,
            false,
        );
        assert!(is_tool_allowed("read_file", &d));
        assert!(is_tool_allowed("grep", &d));
        assert!(!is_tool_allowed("write_file", &d));
        assert!(!is_tool_allowed("bash", &d));
    }

    #[test]
    fn blacklist_removes_tools() {
        let d = def(
            None,
            Some(vec!["bash".to_string(), "write_file".to_string()]),
            false,
        );
        assert!(!is_tool_allowed("bash", &d));
        assert!(!is_tool_allowed("write_file", &d));
        assert!(is_tool_allowed("read_file", &d));
    }

    #[test]
    fn no_filters_inherits_all() {
        let d = def(None, None, false);
        assert!(is_tool_allowed("read_file", &d));
        assert!(is_tool_allowed("bash", &d));
        assert!(is_tool_allowed("agent", &d));
    }

    #[test]
    fn blacklist_wins_over_whitelist() {
        // When both are set, blacklist takes precedence over the whitelist.
        let d = def(
            Some(vec!["read_file".to_string(), "bash".to_string()]),
            Some(vec!["bash".to_string()]),
            false,
        );
        assert!(is_tool_allowed("read_file", &d));
        assert!(!is_tool_allowed("bash", &d));
        assert!(!is_tool_allowed("grep", &d));
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
    fn capability_tag_read_only_when_write_and_bash_disallowed() {
        // The bundled `plan`/`explore` profile: explicit read-only allowlist,
        // write/exec tools disallowed. Must advertise `read-only` so the parent
        // model does not delegate write work here (thread 6cd3d096).
        let d = def(
            Some(vec!["read_file".to_string(), "grep".to_string()]),
            Some(vec![
                "write_file".to_string(),
                "edit_file".to_string(),
                "bash".to_string(),
            ]),
            false,
        );
        assert_eq!(capability_tag(&d), "read-only");
    }

    #[test]
    fn capability_tag_reflects_write_and_bash_availability() {
        let write_only = def(Some(vec!["write_file".to_string()]), None, false);
        assert_eq!(capability_tag(&write_only), "write");
        let bash_only = def(Some(vec!["bash".to_string()]), None, false);
        assert_eq!(capability_tag(&bash_only), "bash");
        let both = def(
            Some(vec!["write_file".to_string(), "bash".to_string()]),
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
        assert_eq!(
            last_assistant_text(&msgs),
            "sub-agent ended without producing a final message"
        );
        // Truly no text anywhere → same sentinel, never an empty string.
        let msgs: Vec<Message> = vec![Message::user_with_content(vec![])];
        assert_eq!(
            last_assistant_text(&msgs),
            "sub-agent ended without producing a final message"
        );
    }
}
