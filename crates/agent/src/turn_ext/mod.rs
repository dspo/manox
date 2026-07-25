//! In-process turn extension points that decouple cross-cutting concerns from
//! the `Thread::run_turn_loop` core. Extensions are plain Rust trait objects
//! registered at construction — there is no dynamic loading and no external
//! extension surface (that axis is `crate::hook`, which shells out to
//! user-configured commands and cannot rewrite the data flow).
//!
//! Three orthogonal hook shapes, all invoked synchronously under `&mut App`
//! against a read-only [`TurnCtx`] snapshot so they never re-enter the `Thread`
//! entity (avoiding gpui double-lease). Async execution (the LLM call, tool
//! runs, the compaction side-call) stays owned by the core loop:
//!
//! - [`ContextInjector`] contributes fixed-slot request messages. Its output
//!   is byte-stable across turns for a given (mode, cwd, files) input — the
//!   prefix-cache contract. Injectors run in registration order at one
//!   deterministic slot right after the system head.
//! - [`CompactionPolicy`] decides whether/where to compact (pure judgement).
//!   The core still owns the async `perform_compaction`.
//! - [`ToolGate`] renders a synchronous, statically-decidable verdict for a
//!   tool call. It does not subsume the Danger / always-allow / reviewer
//!   short-circuits, which depend on turn-local state and the async reviewer;
//!   [`ToolDecision::Defer`] falls back to the core's default gate.
//!
//! PR1 wires only [`ContextInjector`] into the core (`build_completion_request`
//! routes its fixed slot through [`TurnExtensions::inject_all`]). The gate and
//! compaction hooks are defined and registered but not yet consulted by the
//! loop — they land in later PRs. The [`CoreContextInjector`] forwards the
//! previously-inlined CLAUDE.md + collaboration-mode logic verbatim, so the
//! request stays byte-identical.

use std::path::Path;
use std::sync::Arc;

use crate::language::Language;
use crate::language_model::{
    LanguageModelRequestMessage, LanguageModelToolUse, MessageContent, Role,
};
use crate::message::Message;
use crate::thread::ApprovalMode;
use crate::tool::AgentTool;

/// Read-only view of the turn's `Thread` state, built once per hook point.
/// Extensions read this instead of holding a `WeakEntity<Thread>` and
/// re-entering the entity from a hook.
pub struct TurnCtx<'a> {
    pub messages: &'a [Message],
    pub approval_mode: ApprovalMode,
    pub depth: u32,
    pub cwd: &'a Path,
    pub agent_language: Language,
    pub turn_count: u32,
    /// Whether the multi-level CLAUDE.md eager block is injected. Off only for
    /// the built-in `Explore` sub-agent.
    pub instructions_enabled: bool,
}

/// A tool-gate verdict. `Defer` yields to the core's default approval path
/// (Danger / always-allow / reviewer), which a gate never encodes itself.
pub enum ToolDecision {
    Allow,
    Deny { reason: String },
    Defer,
}

/// Synchronous, statically-decidable tool verdict rendered before execution.
pub trait ToolGate: 'static {
    fn decide(
        &self,
        tu: &LanguageModelToolUse,
        tool: &dyn AgentTool,
        ctx: &TurnCtx,
    ) -> ToolDecision;
}

/// Contributes fixed-slot messages to the completion request. The returned
/// messages land immediately after the system head in registration order; an
/// empty result contributes nothing and leaves the request prefix
/// byte-identical to a session without this injector.
pub trait ContextInjector: 'static {
    fn inject(&self, ctx: &TurnCtx) -> Vec<LanguageModelRequestMessage>;
}

/// Decides the compaction insertion point for the upcoming request, or `None`
/// to skip. Pure judgement — the async summarization side-call stays in the
/// core loop.
pub trait CompactionPolicy: 'static {
    fn target_ix(&self, ctx: &TurnCtx) -> Option<usize>;
}

/// The per-thread registry of turn extensions. Constructed by
/// [`main_extensions`] for a depth-0 main thread and [`child_extensions`] for a
/// sub-agent, mirroring the split between `tools::main_registry` and the
/// sub-agent registry.
pub struct TurnExtensions {
    /// Ordered gates; the first non-`Defer` verdict wins. Not yet consulted by
    /// the loop (PR4).
    pub gates: Vec<Arc<dyn ToolGate>>,
    /// Ordered injectors; their contributions are concatenated at the fixed
    /// slot in registration order.
    pub injectors: Vec<Arc<dyn ContextInjector>>,
    /// Exactly one compaction policy. Not yet consulted by the loop (PR3).
    pub compaction: Arc<dyn CompactionPolicy>,
}

impl TurnExtensions {
    /// Run the injectors in order and concatenate their contributions.
    pub fn inject_all(&self, ctx: &TurnCtx) -> Vec<LanguageModelRequestMessage> {
        let mut out = Vec::new();
        for injector in &self.injectors {
            out.extend(injector.inject(ctx));
        }
        out
    }
}

/// A main thread's turn extensions. The fixed-slot injector forwards the
/// CLAUDE.md eager block and the `<collaboration_mode>` block; both gate
/// themselves on the same conditions the inlined code used, so registering the
/// injector for every thread keeps behavior identical.
pub fn main_extensions() -> TurnExtensions {
    TurnExtensions {
        gates: Vec::new(),
        injectors: vec![Arc::new(CoreContextInjector)],
        compaction: Arc::new(CoreCompactionPolicy),
    }
}

/// A sub-agent's turn extensions. A sub-agent (depth > 0) still receives the
/// CLAUDE.md block when `instructions_enabled`, and the injector's own
/// `depth == 0` guard omits the collaboration-mode block — so the same
/// injector serves both, matching the single inlined code path it replaced.
pub fn child_extensions() -> TurnExtensions {
    TurnExtensions {
        gates: Vec::new(),
        injectors: vec![Arc::new(CoreContextInjector)],
        compaction: Arc::new(CoreCompactionPolicy),
    }
}

/// Identity injector: forwards the exact fixed-slot messages
/// `build_completion_request` emitted inline before the extension seam —
/// multi-level CLAUDE.md eager block (gated on `instructions_enabled`), then
/// the `<collaboration_mode>` block (gated on `depth == 0`), in that order.
struct CoreContextInjector;

impl ContextInjector for CoreContextInjector {
    fn inject(&self, ctx: &TurnCtx) -> Vec<LanguageModelRequestMessage> {
        let mut out = Vec::new();
        if ctx.instructions_enabled
            && let Some(text) = crate::claude_md::render_eager(
                &crate::claude_md::load(ctx.cwd, &crate::settings::claude_md_load_context()),
                ctx.agent_language,
            )
        {
            out.push(LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(text)],
                cache: true,
            });
        }
        if ctx.depth == 0 {
            let instructions = crate::collaboration_mode::unified_instructions(ctx.agent_language);
            out.push(LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(format!(
                    "<collaboration_mode>\n{instructions}\n</collaboration_mode>"
                ))],
                cache: true,
            });
        }
        out
    }
}

/// Identity compaction policy: contributes no target, so the core's
/// `Thread::auto_compaction_target` remains the sole decision-maker until PR3
/// moves that logic here.
struct CoreCompactionPolicy;

impl CompactionPolicy for CoreCompactionPolicy {
    fn target_ix(&self, _ctx: &TurnCtx) -> Option<usize> {
        None
    }
}
