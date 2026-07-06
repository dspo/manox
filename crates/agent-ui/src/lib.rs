//! manox UI layer, built on gpui-component.
//!
//! Workspace top-level view + `ConversationState` + views. Holds an
//! `Entity<agent::Thread>` and subscribes to `ThreadEvent` for incremental rendering.

pub mod conversation;
pub mod dispatch;
pub mod slash_command;
pub mod views;
pub mod workspace;

pub use views::settings::SettingsView;
pub use workspace::Workspace;

// Open/close the right-side markdown composer, plus the global OpenSettings
// action that flips the Workspace into the Settings overlay.
gpui::actions!(
    agent_ui,
    [ToggleEditor, ToggleEditorPreview, CloseEditor, OpenSettings]
);
