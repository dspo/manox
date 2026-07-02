//! manox UI layer, built on gpui-component.
//!
//! Workspace top-level view + `ConversationState` + views. Holds an
//! `Entity<agent::Thread>` and subscribes to `ThreadEvent` for incremental rendering.

pub mod conversation;
pub mod editor;
pub mod views;
pub mod workspace;

pub use workspace::Workspace;

// Open/close the right-side markdown composer.
gpui::actions!(agent_ui, [ToggleEditor, SubmitEditor]);
