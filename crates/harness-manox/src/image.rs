//! Clipboard image -> provider-ready `MessageContent::Image`.
//!
//! The implementation moved to `agent::image` (shared with the pi harness);
//! this module keeps the archived import path alive.

pub use agent::image::gpui_image_to_message_content;
