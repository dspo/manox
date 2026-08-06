//! manox UI layer, built on gpui-component.
//!
//! Workspace top-level view + `ConversationState` + views. Holds an
//! `Entity<agent::Thread>` and subscribes to `ThreadEvent` for incremental rendering.
//!
//! Harness selection: `harness-manox` (default) mounts the manox harness;
//! `harness-pi` mounts the `pi_backend` adapter over crates/pi. Exactly one
//! must be enabled.

// Exactly one harness backend must be selected (`harness-pi` is the
// default; `harness-manox` is the retired archive variant).
#[cfg(all(feature = "harness-manox", feature = "harness-pi"))]
compile_error!(
    "features `harness-manox` and `harness-pi` are mutually exclusive; enable exactly one"
);
#[cfg(not(any(feature = "harness-manox", feature = "harness-pi")))]
compile_error!("enable one of the harness features: `harness-pi` (default) or `harness-manox`");

pub mod assets;
pub mod browser_host;
pub mod chatgpt_app;
pub mod cockpit;
pub mod conversation;
pub mod dispatch;
pub mod external_session;
pub mod git_status;
// TODO(pi-wire): slash commands — backed by the manox command/skill registries.
#[cfg(feature = "harness-manox")]
pub mod slash_command;
pub mod views;
pub mod workspace;

pub use views::settings::SettingsView;
pub use workspace::Workspace;

pub use chatgpt_app::LaunchChatGptApp;

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
