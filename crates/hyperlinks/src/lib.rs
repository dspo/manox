//! Pure link detection shared by the terminal grid and the message renderer.
//!
//! The crate is the single source of truth for what counts as a clickable
//! span: multi-protocol URLs (`url.rs`) and filesystem paths with optional
//! `path:line:col` anchors (`path.rs`). It is dependency-free and works on
//! plain `&str` with byte ranges, so both the terminal (grid cells assembled
//! into text) and the markdown renderer (flat inline text) consume the same
//! rules.
//!
//! Overlapping spans are resolved by [`consolidate`], which prefers URLs over
//! paths and keeps the longer span when two start at the same offset. OSC 8
//! hyperlinks are deliberately out of scope — the terminal cell layer owns
//! those.

mod path;
mod shared;
mod url;

pub use path::{PathOptions, default_path_options, detect_paths, is_path_like};
pub use shared::{OverlaySpan, UrlKind, consolidate, is_covered};
pub use url::{SCHEMES, detect_urls, trim_url};

#[cfg(test)]
mod tests;
