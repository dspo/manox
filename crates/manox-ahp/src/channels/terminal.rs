//! The terminal channel (`ahp-terminal:/<id>`).
//!
//! AHP gives terminals a real channel with a claim (which client owns input)
//! and command detection, which is strictly more than the v2 follow stream
//! offered: several clients may observe one terminal, and ownership moves by
//! dispatching `terminal/claimed`.

/// The terminal URI scheme prefix.
pub const SCHEME: &str = "ahp-terminal:/";

/// `ahp-terminal:/<id>`.
pub fn uri(id: &str) -> String {
    format!("{SCHEME}{id}")
}

/// The terminal id inside an `ahp-terminal:/<id>` URI.
pub fn id(uri: &str) -> Option<&str> {
    uri.strip_prefix(SCHEME).filter(|id| !id.is_empty())
}
