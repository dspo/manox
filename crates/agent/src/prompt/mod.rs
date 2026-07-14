//! Prompt template rendering — the single boundary between model-visible
//! prose and Rust flow code.
//!
//! Model-visible prompts live as `.tera.md` text files under `templates/`,
//! embedded at compile time. Business code constructs typed
//! [`data`] payloads (or [`draft`]s) and hands them to [`renderer::render`],
//! which is the only place outside tests that touches `tera::`. No `format!` /
//! `push_str` / `replace()` of model prose is permitted outside this module.
//!
//! Layout:
//! - [`template`] — the [`PromptTemplate`] enum (every built-in key) + `ALL`.
//! - [`data`] — `#[derive(Serialize)]` payloads, one per templated prompt.
//! - [`renderer`] — the global `Tera`, `render` / `render_message` /
//!   `render_tool`.
//! - [`draft`] — un-rendered shapes (`PromptDraft` / `PromptMessageDraft` /
//!   `PromptToolDraft`) carried across non-boundary function boundaries.

pub mod data;
pub mod draft;
pub mod renderer;
pub mod template;

pub use data::*;
pub use draft::{PromptDraft, PromptMessageDraft, PromptToolDraft};
pub use renderer::{
    render, render_command_body, render_message, render_static, render_tool, render_user_message,
};
pub use template::{ALL, PromptTemplate};
