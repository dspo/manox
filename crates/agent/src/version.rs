//! Compile-time version and commit information, captured by build.rs.

/// From the workspace `Cargo.toml` version field.
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Full git commit SHA captured at build time, if git was available.
pub const COMMIT_SHA: Option<&str> = option_env!("MANOX_COMMIT_SHA");

fn build_type() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// Human-readable version identifier (`"manox 0.1.0 (debug)"`).
pub fn full_version_string() -> String {
    format!("manox {PKG_VERSION} ({})", build_type())
}

/// Multi-line build information block copied to the clipboard from the
/// About window. Line format is load-bearing; keep it stable.
pub fn structured_about() -> String {
    let commit = COMMIT_SHA.unwrap_or("unknown");
    let mut block = format!(
        "Manox {PKG_VERSION} ({})\ncommit: {commit}\nos: {}\narch: {}",
        build_type(),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    if let Some(rustc) = option_env!("RUSTC_VERSION") {
        block.push_str(&format!("\nrustc: {rustc}"));
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_about_lists_version_commit_os_arch() {
        let block = structured_about();
        let mut lines = block.lines();
        assert!(
            lines
                .next()
                .is_some_and(|l| l.starts_with(&format!("Manox {PKG_VERSION} (")))
        );
        assert!(lines.next().is_some_and(|l| l.starts_with("commit: ")));
        assert_eq!(
            lines.next(),
            Some(format!("os: {}", std::env::consts::OS).as_str())
        );
        assert_eq!(
            lines.next(),
            Some(format!("arch: {}", std::env::consts::ARCH).as_str())
        );
    }
}
