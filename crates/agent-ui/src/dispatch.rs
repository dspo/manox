//! Process-global handles for dispatching actions from outside the view tree.
//!
//! macOS system menu items are evaluated against App-level `on_action`
//! handlers, not the view tree's local listeners. Dispatching the
//! `OpenSettings` action through such a handler therefore needs a way to
//! reach the active window's `Workspace` from `&mut App`. Stashing the entity
//! and the window handle here at window creation time gives the App-level
//! handler a stable handle (the `cx.active_window()` handle from a menu
//! callback is unreliable on macOS — it can point at a `WindowId` that the
//! App's window map has not registered yet, surfacing as `Err(window not
//! found)`).
//!
//! Both slots are populated exactly once for the single main window the
//! process opens. If a future change ever supports multiple windows, the
//! `OnceLock` registration will need to be replaced with a slot map keyed
//! by `WindowId` and the App-level handler will need to pick the target
//! window (e.g. from `cx.active_window()` after the deferred dispatch).

use std::sync::OnceLock;

use gpui::{Entity, WindowHandle};

use crate::workspace::Workspace;
use gpui_component::Root;

static WORKSPACE: OnceLock<Entity<Workspace>> = OnceLock::new();
static WINDOW: OnceLock<WindowHandle<Root>> = OnceLock::new();

/// Register the single main `Workspace` entity. Call once, from inside
/// `cx.open_window`'s build-root callback after `cx.new(|cx| Workspace::new(...))`.
pub fn set_workspace(workspace: Entity<Workspace>) {
    let _ = WORKSPACE.set(workspace);
}

/// Register the main window's typed `WindowHandle<Root>`. Call once, with the
/// value returned by `cx.open_window(...)`.
pub fn set_window(window: WindowHandle<Root>) {
    let _ = WINDOW.set(window);
}

/// Returns the global `Workspace` entity, or `None` if the main window has
/// not been opened yet.
pub fn workspace_global() -> Option<Entity<Workspace>> {
    WORKSPACE.get().cloned()
}

/// Returns the main window's typed handle, or `None` if it has not been
/// opened yet.
pub fn window_global() -> Option<WindowHandle<Root>> {
    WINDOW.get().cloned()
}
