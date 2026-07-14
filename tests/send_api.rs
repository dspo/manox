//! Integration test that consumes `cx` as an external crate would: exercising the
//! public `send` API via `cx::` paths, so the export surface is verified to
//! compile from outside the crate (not just via `crate::`).

// Both the module path and the root re-export resolve; `cx::send` is a module
// (type namespace) and `cx::send` is also the re-exported function (value
// namespace) — Rust keeps them apart, so `use cx::send;` imports the module and
// `cx::send(...)` calls the function.
use cx::send;

#[test]
fn send_rejects_empty_without_clear_via_public_api() {
    // module-qualified call form
    let err = send::send(&send::SendSelector::Latest, None, false).unwrap_err();
    assert!(format!("{err}").contains("--clear-buffer"));

    // root re-exported call form (value namespace) and types (type namespace)
    let err2 = cx::send(&cx::SendSelector::Latest, None, false).unwrap_err();
    assert!(format!("{err2}").contains("--clear-buffer"));
}
