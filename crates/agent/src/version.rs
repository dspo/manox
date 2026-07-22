//! Compile-time version and commit information, captured by build.rs.

/// From the workspace `Cargo.toml` version field.
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Full git commit SHA captured at build time, if git was available.
pub const COMMIT_SHA: Option<&str> = option_env!("MANOX_COMMIT_SHA");

/// Short (7-char) commit SHA prefix.
pub fn commit_short() -> Option<&'static str> {
    COMMIT_SHA.map(|s| &s[..7.min(s.len())])
}

/// Single-line human-readable version identifier for the About window and
/// settings panel.
pub fn full_version_string() -> String {
    let build_type = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let mut s = format!("manox {PKG_VERSION} ({build_type})");
    if let Some(sha) = COMMIT_SHA {
        s.push_str(&format!("\ncommit: {sha}"));
    }
    s
}
