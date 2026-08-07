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
//! The process keeps a single main window, but that window may be closed and
//! re-opened (the system-tray "open" path): `WORKSPACE` is populated exactly
//! once and held for the process lifetime — the strong `Entity` reference is
//! what keeps the foreground `Thread` and any parked background threads alive
//! while no window exists — while `WINDOW` is replaced each time a new window
//! is opened over the same `Workspace`. A stale `WindowHandle` read between
//! close and re-open simply fails its `update`, which App-level handlers
//! already tolerate. If a future change ever supports multiple windows, these
//! slots will need to become a map keyed by `WindowId` and the App-level
//! handlers will need to pick the target window (e.g. from `cx.active_window()`
//! after the deferred dispatch).

use std::sync::{OnceLock, RwLock};

use gpui::{Entity, WindowHandle};

use crate::workspace::Workspace;
use gpui_component::Root;

static WORKSPACE: OnceLock<Entity<Workspace>> = OnceLock::new();
static WINDOW: RwLock<Option<WindowHandle<Root>>> = RwLock::new(None);

/// Register the single main `Workspace` entity. Call once, from inside
/// `cx.open_window`'s build-root callback after `cx.new(|cx| Workspace::new(...))`.
/// Later windows are re-opened over this same entity; the registration is
/// deliberately process-lifetime so the workspace (and the threads it holds)
/// survives the window being closed.
pub fn set_workspace(workspace: Entity<Workspace>) {
    let _ = WORKSPACE.set(workspace);
}

/// Register the main window's typed `WindowHandle<Root>`. Replaces any
/// previous handle — called each time the main window is (re-)opened, so the
/// handle always tracks the live window even across a close/re-open cycle.
pub fn set_window(window: WindowHandle<Root>) {
    let mut slot = WINDOW.write().expect("dispatch WINDOW lock poisoned");
    *slot = Some(window);
}

/// Returns the global `Workspace` entity, or `None` if the main window has
/// not been opened yet.
pub fn workspace_global() -> Option<Entity<Workspace>> {
    WORKSPACE.get().cloned()
}

/// Returns the main window's typed handle, or `None` if no main window is
/// currently open. The handle may refer to a window that has since been
/// closed; callers treat a failed `update` as "no window".
pub fn window_global() -> Option<WindowHandle<Root>> {
    *WINDOW.read().expect("dispatch WINDOW lock poisoned")
}
