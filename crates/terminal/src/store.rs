//! Process-global `TerminalStore` — mirrors `agent::thread_store`.
//!
//! Stage 7 implements session summaries + `save_terminal` against the shared
//! `ThreadsDatabase`. Stage 0 only registers the empty store so `terminal::init`
//! has a stable call site.

use gpui::App;

pub fn init(_cx: &mut App) {}
