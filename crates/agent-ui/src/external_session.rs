//! External agent CLI sessions (claude / codex / copilot) launched from the
//! sidebar's `+` menu.
//!
//! An `ExternalSession` owns a `TerminalView` rendering the agent's TUI (driven
//! through a `CxSessionSource` PTY source that wraps the shared
//! `cx::SessionHandle`) plus the `Arc<SessionHandle>` itself, so the close path
//! can `kill` the agent explicitly. Sessions live only in memory — they are not
//! persisted and vanish from the sidebar on exit.

use std::path::PathBuf;
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

    /// Embedded SVG asset path (resolved by `ExtrasAssetSource`) for the
    /// agent's brand icon. The SVGs use `fill="currentColor"` so the caller
    /// tints via `.text_color(...)`.
    pub fn icon_asset(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "icons/claude.svg",
            Self::Codex => "icons/codex.svg",
            Self::GithubCopilot => "icons/githubcopilot.svg",
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
/// It lives on the session so an explicit close detaches it first; once
/// detached, the killed child's eventual reap emits `ChildExit` into a
/// listener set that no longer holds this observer, so the close path is the
/// sole remover and there is no double-removal.
pub struct ExternalSession {
    pub id: String,
    pub kind: SessionKind,
    /// Epoch seconds at spawn time. The sidebar sort key so an external
    /// session mixes into the Conversations list by recency alongside manox
    /// threads (which sort by `interacted_at`). manox cannot observe model
    /// switches inside the TUI, let alone inter-message timing, so the spawn
    /// time is the only stable ordering signal it has.
    pub created_at: i64,
    /// The project path the session was bound to at spawn (`Some` when launched
    /// from a project folder's `+` button, `None` from the Conversations
    /// header). The sidebar uses it to group the row under its project folder
    /// instead of in the loose Conversations list, matching how manox threads
    /// bound to a project are grouped.
    pub project: Option<PathBuf>,
    pub terminal_view: Entity<TerminalView>,
    pub handle: Arc<cx::SessionHandle>,
    pub _exit_sub: Subscription,
}

impl ExternalSession {
    /// Display line for the sidebar row: `<kind label>`.
    pub fn title(&self) -> &str {
        self.kind.label()
    }

    /// The lightweight descriptor the sidebar renders from. The sidebar is a
    /// separate Entity from the Workspace that owns the live `ExternalSession`
    /// (with its `TerminalView` + `Arc<SessionHandle>`); it only needs identity
    /// and display fields to render a row. The spawn-time provider/model are
    /// intentionally not projected: the user can switch models inside the TUI
    /// and manox cannot observe that, so showing them would mislead.
    pub fn summary(&self) -> ExternalSessionSummary {
        ExternalSessionSummary {
            id: self.id.clone(),
            kind: self.kind,
            created_at: self.created_at,
            project: self.project.clone(),
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
    pub created_at: i64,
    pub project: Option<PathBuf>,
}
