//! manox UI layer, built on gpui-component.
//!
//! Workspace top-level view + `ConversationState` + views. Holds an
//! `Entity<agent::Thread>` and subscribes to `ThreadEvent` for incremental rendering.

pub mod assets;
pub mod browser_host;
pub mod cockpit;
pub mod conversation;
pub mod dispatch;
pub mod external_session;
pub mod git_status;
#[cfg(feature = "debug")]
pub mod harness;
pub mod slash_command;
pub mod views;
pub mod workspace;

pub use views::settings::SettingsView;
pub use workspace::Workspace;

// Open/close the right-side markdown composer, plus the global OpenSettings
// action that flips the Workspace into the Settings overlay. AskPrev/AskNext
// navigate between questions in the ask drawer (bound to arrow keys within the
// drawer's focus context).
gpui::actions!(
    agent_ui,
    [
        ToggleEditor,
        ToggleEditorPreview,
        CloseEditor,
        OpenSettings,
        AskPrev,
        AskNext,
        AskCancel,
        NewTerminalTab,
        CloseTerminalTab,
        FocusTerminal,
        FocusConversation,
        OpenBrowserTab,
        CloseBrowserTab,
        CompletionUp,
        CompletionDown,
        CompletionConfirm,
        CompletionDismiss,
        UndoLastQueued,
        ToggleCockpitTasks,
        BackgroundCurrentThread,
        CycleCollaborationMode
    ]
);
