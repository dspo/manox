//! manox UI layer, built on gpui-component.
//!
//! Workspace top-level view + `ConversationState` + views. Holds an
//! `Entity<agent::Thread>` and subscribes to `ThreadEvent` for incremental rendering.
//!

pub mod assets;
pub mod browser_host;
pub mod chatgpt_app;
pub mod cockpit;
pub mod conversation;
pub mod dispatch;
pub mod external_session;
pub mod git_status;
pub mod overlap_diag;
pub mod slash_command;
pub mod views;
pub mod vscode_app;
pub mod workspace;

pub use views::settings::SettingsView;
pub use workspace::Workspace;

pub use chatgpt_app::LaunchChatGptApp;
pub use vscode_app::LaunchVSCode;

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
        ComposerRecallUp,
        ComposerRecallDown,
        UndoLastQueued,
        ToggleCockpitTasks,
        BackgroundCurrentThread,
        ToggleTurnNavigator,
        CopySelectedTurn,
        ArchiveCurrentThread
    ]
);

/// Keybindings shadowing the composer Input's own up/down while it is
/// focused; the handlers defer to native caret movement unless a history
/// recall is eligible. Registered after `gpui_component::init`, so the
/// same-depth `composer == recall > Input` predicate wins the tie.
pub fn composer_recall_key_bindings() -> Vec<gpui::KeyBinding> {
    vec![
        gpui::KeyBinding::new("up", ComposerRecallUp, Some("composer == recall > Input")),
        gpui::KeyBinding::new(
            "down",
            ComposerRecallDown,
            Some("composer == recall > Input"),
        ),
    ]
}

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

#[cfg(test)]
mod tests {
    use super::{composer_recall_key_bindings, turn_navigator_key_bindings};

    /// `KeyBinding::new` panics on an unparseable context predicate, so
    /// constructing every binding set is a startup crash regression test.
    #[test]
    fn key_bindings_parse() {
        assert_eq!(composer_recall_key_bindings().len(), 2);
        assert_eq!(turn_navigator_key_bindings().len(), 6);
    }
}
