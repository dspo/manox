//! Bundle hygiene: the shipped entitlements and the generated Info.plist must
//! not declare broad Apple Events / media intent by default. These guard
//! against regression on the privacy posture — manox.app no longer prompts for
//! "access Apple Music / media activity" because it neither declares
//! `automation.apple-events` nor ships an `NSAppleEventsUsageDescription`.

use std::path::PathBuf;

const ENTITLEMENTS: &str = include_str!("../resources/manox.entitlements");

/// Path to the bundle script, resolved from the crate root so the test works
/// regardless of the working directory cargo runs from.
fn bundle_script() -> PathBuf {
    let crate_root = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(crate_root)
        .join("../../script/bundle-mac")
        // Joining keeps it filesystem-agnostic; the test only reads the file.
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(crate_root).join("../../script/bundle-mac"))
}

#[test]
fn entitlements_drop_apple_events() {
    assert!(
        !ENTITLEMENTS.contains("com.apple.security.automation.apple-events"),
        "Apple Events entitlement must be removed from the default bundle: {ENTITLEMENTS}"
    );
}

#[test]
fn entitlements_keep_gpui_runtime_keys() {
    assert!(
        ENTITLEMENTS.contains("com.apple.security.cs.allow-jit"),
        "GPUI JIT entitlement must remain: {ENTITLEMENTS}"
    );
    assert!(
        ENTITLEMENTS.contains("com.apple.security.cs.allow-unsigned-executable-memory"),
        "GPUI unsigned-executable-memory entitlement must remain: {ENTITLEMENTS}"
    );
}

#[test]
fn bundle_script_drops_apple_events_usage_description() {
    let script =
        std::fs::read_to_string(bundle_script()).expect("script/bundle-mac must be readable");
    assert!(
        !script.contains("NSAppleEventsUsageDescription"),
        "Info.plist generation must not write NSAppleEventsUsageDescription: {script}"
    );
    assert!(
        !script.contains("NSAppleMusicUsageDescription"),
        "no media-library usage description should be added: {script}"
    );
}
