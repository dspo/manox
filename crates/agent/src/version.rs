//! Compile-time version and commit information, captured by build.rs.

/// From the workspace `Cargo.toml` version field.
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Full git commit SHA captured at build time, if git was available.
pub const COMMIT_SHA: Option<&str> = option_env!("MANOX_COMMIT_SHA");

/// Human-readable version identifier (`"manox 0.1.0 (debug)"`).
pub fn full_version_string() -> String {
    let build_type = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    format!("manox {PKG_VERSION} ({build_type})")
}
