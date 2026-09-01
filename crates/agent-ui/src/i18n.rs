//! GPUI-aware i18n helpers — thin wrappers over `manox_agent::i18n` that return
//! `gpui::SharedString` for use in gpui element/component APIs.

pub use manox_agent::i18n::{Language, set_ui_language};
use gpui::SharedString;

/// Resolve `key` with no arguments as a `SharedString`.
pub fn t(key: &str) -> SharedString {
    manox_agent::i18n::t(key).into()
}

/// Resolve `key` with string arguments as a `SharedString`.
pub fn t_str(key: &str, args: &[(&str, &str)]) -> SharedString {
    manox_agent::i18n::t_str(key, args).into()
}

/// Resolve `key` with a numeric `$count` argument as a `SharedString`.
pub fn t_count(key: &str, count: i64) -> SharedString {
    manox_agent::i18n::t_count(key, count).into()
}

/// Resolve `key` with string arguments plus a numeric `$count` as a `SharedString`.
pub fn t_str_count(key: &str, args: &[(&str, &str)], count: i64) -> SharedString {
    manox_agent::i18n::t_str_count(key, args, count).into()
}
