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
        CycleCollaborationMode,
        ToggleTurnNavigator,
        CopySelectedTurn,
        ArchiveCurrentThread
    ]
);

/// Keybindings owned by the user-turn navigator.
///
/// Keeping these beside the actions lets the application and GPUI interaction
/// tests install the exact same bindings. The descendant context is more
/// specific than the input's own bindings, so navigation keys are intercepted
/// only while the navigator search field is focused.
pub fn turn_navigator_key_bindings() -> Vec<gpui::KeyBinding> {
    vec![
        #[cfg(target_os = "macos")]
        gpui::KeyBinding::new("cmd-m", ToggleTurnNavigator, None),
        #[cfg(not(target_os = "macos"))]
        gpui::KeyBinding::new("ctrl-m", ToggleTurnNavigator, None),
        #[cfg(target_os = "macos")]
        gpui::KeyBinding::new("cmd-c", CopySelectedTurn, Some("TurnNavigator > Input")),
        #[cfg(not(target_os = "macos"))]
        gpui::KeyBinding::new("ctrl-c", CopySelectedTurn, Some("TurnNavigator > Input")),
        gpui::KeyBinding::new("up", CompletionUp, Some("TurnNavigator > Input")),
        gpui::KeyBinding::new("down", CompletionDown, Some("TurnNavigator > Input")),
        gpui::KeyBinding::new("enter", CompletionConfirm, Some("TurnNavigator > Input")),
        gpui::KeyBinding::new("escape", CompletionDismiss, Some("TurnNavigator > Input")),
    ]
}
