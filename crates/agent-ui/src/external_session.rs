//! External agent CLI sessions (claude / codex / copilot) launched from the
//! sidebar's `+` menu.
//!
//! An `ExternalSession` owns a `TerminalView` rendering the agent's TUI (driven
//! through a `CxSessionSource` PTY source that wraps the shared
//! `cx::SessionHandle`) plus the `Arc<SessionHandle>` itself, so the close path
//! can `kill` the agent explicitly. Sessions live only in memory — they are not
//! persisted and vanish from the sidebar on exit.

use std::sync::Arc;

use gpui::{Entity, Subscription};

use terminal_ui::TerminalView;

/// Which external agent CLI a session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    ClaudeCode,
    Codex,
    GithubCopilot,
}

impl SessionKind {
    /// The sidebar row label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::GithubCopilot => "GitHub Copilot",
        }
    }

    /// The cx `Agent` to launch.
    pub fn agent(&self) -> cx::Agent {
        match self {
            Self::ClaudeCode => cx::Agent::Claude,
            Self::Codex => cx::Agent::Codex,
            Self::GithubCopilot => cx::Agent::Copilot,
        }
    }

    /// The cx agent id matching `ResolvedModel.visible_agents` entries, used to
    /// filter the model list to those that can drive this agent.
    pub fn agent_id(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::GithubCopilot => "copilot",
        }
    }
}

/// A live external agent CLI session. The `TerminalView` renders the agent's
/// TUI; the shared `handle` lets the close path `kill` the agent (the terminal
/// view holds the `CxSessionSource` that drives IO, this clone holds the
/// kill-capable reference so closing the tab doesn't orphan the child).
///
/// The `id` is namespaced `external:<kind>:<uuid>` so it never collides with a
/// manox thread UUID in the sidebar's selection namespace.
///
/// `_exit_sub` observes the terminal's `ChildExit` event so a natural CLI exit
/// (e.g. `/exit` in claude) tears the session down without the user clicking ×.
/// It lives on the session so dropping the session detaches it, preventing any
/// spurious post-close event from firing into a freed `ExternalSession`.
pub struct ExternalSession {
    pub id: String,
    pub kind: SessionKind,
    pub provider_name: String,
    pub model_id: String,
    pub terminal_view: Entity<TerminalView>,
    pub handle: Arc<cx::SessionHandle>,
    pub _exit_sub: Subscription,
}

impl ExternalSession {
    /// Display line for the sidebar row: `<kind label>` (the provider/model is
    /// shown beneath it as a muted subtitle).
    pub fn title(&self) -> &str {
        self.kind.label()
    }

    /// The lightweight descriptor the sidebar renders from. The sidebar is a
    /// separate Entity from the Workspace that owns the live `ExternalSession`
    /// (with its `TerminalView` + `Arc<SessionHandle>`); it only needs identity
    /// + display fields to render a row.
    pub fn summary(&self) -> ExternalSessionSummary {
        ExternalSessionSummary {
            id: self.id.clone(),
            kind: self.kind,
            provider_name: self.provider_name.clone(),
            model_id: self.model_id.clone(),
        }
    }
}

/// Render-only projection of an [`ExternalSession`] handed to the sidebar so it
/// can list external rows without holding PTY handles or terminal views. The
/// Workspace owns the canonical `Vec<ExternalSession>` and pushes a fresh
/// `Vec<ExternalSessionSummary>` snapshot to the sidebar whenever the set
/// changes.
#[derive(Debug, Clone)]
pub struct ExternalSessionSummary {
    pub id: String,
    pub kind: SessionKind,
    pub provider_name: String,
    pub model_id: String,
}
