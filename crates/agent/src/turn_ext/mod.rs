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
//! Only [`ContextInjector`] is wired into the core today
//! (`build_completion_request` routes its fixed slot through
//! [`TurnExtensions::inject_all`]). The gate and compaction hooks are defined
//! and registered but not yet consulted by the loop — they land in later PRs.
//! Two single-purpose injectors carry the fixed slot: [`ClaudeMdInjector`]
//! (multi-level CLAUDE.md eager block) then [`CollaborationModeInjector`] (the
//! `<collaboration_mode>` block), in that registration order — byte-identical
//! to the logic they replaced.

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

/// A main thread's turn extensions. The fixed slot is the CLAUDE.md eager
/// block followed by the `<collaboration_mode>` block, each carried by its own
/// single-purpose injector so registration order is the sole source of slot
/// order.
pub fn main_extensions() -> TurnExtensions {
    TurnExtensions {
        gates: Vec::new(),
        injectors: vec![
            Arc::new(ClaudeMdInjector),
            Arc::new(CollaborationModeInjector),
        ],
        compaction: Arc::new(CoreCompactionPolicy),
    }
}

/// A sub-agent's turn extensions. A sub-agent (depth > 0) still receives the
/// CLAUDE.md block when `instructions_enabled`; [`CollaborationModeInjector`]'s
/// own `depth == 0` guard makes it a no-op here, so registering both injectors
/// matches the single code path they replaced while keeping the two threads'
/// registries structurally identical for later divergence.
pub fn child_extensions() -> TurnExtensions {
    TurnExtensions {
        gates: Vec::new(),
        injectors: vec![
            Arc::new(ClaudeMdInjector),
            Arc::new(CollaborationModeInjector),
        ],
        compaction: Arc::new(CoreCompactionPolicy),
    }
}

/// Injects the multi-level CLAUDE.md eager block as a fixed-slot user message,
/// gated on `instructions_enabled` (off only for the built-in `Explore`
/// sub-agent). Contributes nothing when no instruction files load.
struct ClaudeMdInjector;

impl ContextInjector for ClaudeMdInjector {
    fn inject(&self, ctx: &TurnCtx) -> Vec<LanguageModelRequestMessage> {
        if ctx.instructions_enabled
            && let Some(text) = crate::claude_md::render_eager(
                &crate::claude_md::load(ctx.cwd, &crate::settings::claude_md_load_context()),
                ctx.agent_language,
            )
        {
            return vec![LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(text)],
                cache: true,
            }];
        }
        Vec::new()
    }
}

/// Injects the `<collaboration_mode>` block as a fixed-slot user message, gated
/// on `depth == 0` (main thread only; sub-agents carry their own `agents/*.md`
/// body and run in `Default`).
struct CollaborationModeInjector;

impl ContextInjector for CollaborationModeInjector {
    fn inject(&self, ctx: &TurnCtx) -> Vec<LanguageModelRequestMessage> {
        if ctx.depth != 0 {
            return Vec::new();
        }
        let instructions = crate::collaboration_mode::unified_instructions(ctx.agent_language);
        vec![LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text(format!(
                "<collaboration_mode>\n{instructions}\n</collaboration_mode>"
            ))],
            cache: true,
        }]
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
