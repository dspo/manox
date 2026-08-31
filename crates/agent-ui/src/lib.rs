//! manox UI layer, built on gpui-component.
//!
//! Workspace top-level view + `ConversationState` + views. Holds an
//! the AgentServer-backed `ClientStoreHandle` (the data mirror around the
//! gpui-free `agent::ThreadHandle`) and subscribes to
//! `ThreadEvent` for incremental rendering.
pub mod assets;
pub mod browser_host;
pub mod chatgpt_app;
pub mod client_store;
pub mod client_store_handle;
pub mod cockpit;
pub mod conversation;
pub mod dispatch;
pub mod external_session;
pub mod git_status;
pub mod i18n;
pub mod menu;
pub(crate) mod overlap_diag;
pub mod server_note_translate;
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
        FillComposerTurn,
        ArchiveCurrentThread
    ]
);

/// Keybindings for composer history recall.
///
/// Recall lives on `alt-up` / `alt-down` only: the bare arrows belong to the
/// `Input`'s own `MoveUp` / `MoveDown`, so within the composer they always
/// move the caret and never jump between past turns — at any caret position,
/// including the first line's start and the last line's end. The bindings
/// match the whole composer subtree (`composer > Input`) regardless of the
/// input's content, because nothing native claims these keystrokes; the
/// wrapper stops carrying the `composer` context while the completion
/// popover is open, which is the one state where recall stays out of the way.
pub fn composer_recall_key_bindings() -> Vec<gpui::KeyBinding> {
    vec![
        gpui::KeyBinding::new("alt-up", ComposerRecallUp, Some("composer > Input")),
        gpui::KeyBinding::new("alt-down", ComposerRecallDown, Some("composer > Input")),
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
        // Enter jumps to the selected turn; the secondary modifier refills the
        // composer with it instead (reuse-and-edit, not locate).
        #[cfg(target_os = "macos")]
        gpui::KeyBinding::new("cmd-enter", FillComposerTurn, Some("TurnNavigator > Input")),
        #[cfg(not(target_os = "macos"))]
        gpui::KeyBinding::new(
            "ctrl-enter",
            FillComposerTurn,
            Some("TurnNavigator > Input"),
        ),
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
        assert_eq!(turn_navigator_key_bindings().len(), 7);
    }
}
