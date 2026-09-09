//! Fixture export (§D.8): real frames as JSON files — the exported
//! wire-contract surface (L12) for any client, and a drift gate against
//! the Rust serde shape. Historically consumed by the webui vitest guard
//! suite (T3, J.5 dual-path consistency); the TS twin was removed with the
//! frontend (final state at tag `archive/frontends-final`).
//!
//! Drift gate (review round 3, §二.9): the tests deep-compare the
//! COMMITTED fixture against the freshly generated typed sample — a
//! `serde_json::Value` comparison, which is object-key-order-insensitive
//! (key order is not part of the contract, and the generator's byte order
//! legitimately varies with the build's serde_json feature unification:
//! `-p` runs sort keys, workspace runs keep insertion order) — and they no
//! longer rewrite the committed files on every run. The old model
//! (rewrite, then read back the bytes just written) detected no drift at
//! all and left seven "content-identical, key-reordered" dirty fixtures in
//! the worktree after any suite run. Regeneration is explicit:
//! `MANOX_UPDATE_FIXTURES=1 cargo test -p manox-protocol --test fixtures`
//! rewrites the files; commit the result.
//!
//! The files cover the four stream item classes (`Snapshot` / `Entry` /
//! `Projections` / `StreamEnd`), every journal event type, and every host
//! event.

use std::path::PathBuf;

use manox_protocol::journal::JournalWireEntry;
use manox_protocol::stream::{StreamEndReason, StreamFrame};
use manox_protocol::surface::{frame_samples, header_sample, host_samples, journal_samples};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn update_mode() -> bool {
    std::env::var_os("MANOX_UPDATE_FIXTURES").is_some_and(|v| v == "1")
}

fn render(value: &serde_json::Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).unwrap())
}

/// The drift gate: compare the committed fixture against the fresh sample
/// as parsed values (object-key-order-insensitive); rewrite only in the
/// explicit `MANOX_UPDATE_FIXTURES=1` mode.
fn commit_fixture(name: &str, value: &serde_json::Value) {
    let path = fixtures_dir().join(name);
    if update_mode() {
        std::fs::create_dir_all(fixtures_dir()).expect("create fixtures dir");
        std::fs::write(&path, render(value)).expect("write fixture");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "fixture {name} is missing or unreadable ({e:#}); regenerate with \
             MANOX_UPDATE_FIXTURES=1 cargo test -p manox-protocol --test fixtures"
        )
    });
    let committed_value: serde_json::Value = serde_json::from_str(&committed)
        .unwrap_or_else(|e| panic!("fixture {name} does not parse ({e:#}) — regenerate it"));
    let fresh: serde_json::Value =
        serde_json::from_str(&render(value)).expect("fresh render parses");
    assert_eq!(
        committed_value, fresh,
        "fixture {name} drifted from the Rust declaration surface; regenerate with \
         MANOX_UPDATE_FIXTURES=1 cargo test -p manox-protocol --test fixtures and commit"
    );
}

fn read_back<T: serde::de::DeserializeOwned>(name: &str) -> T {
    let path = fixtures_dir().join(name);
    let text = std::fs::read_to_string(&path).expect("read fixture");
    serde_json::from_str(&text).expect("fixture parses")
}

/// C3/J.5: every declaration table as one JSON artifact. The Rust
/// declaration (macro-generated from the wire enums) is the single source
/// of the wire vocabulary; the former TS guard suite (removed with the
/// frontend) asserted its tag arrays against this file.
#[test]
fn export_surface_tags() {
    use manox_protocol::surface::{
        CLIENT_CALLS, CLIENT_NOTES, HOST_EVENTS, JOURNAL_ENTRIES, PROJECTION_KEYS, SERVER_CALLS,
        SERVER_NOTES, STREAM_END_REASONS, STREAM_FRAMES, STREAM_KINDS,
    };
    let value = serde_json::json!({
        "journalEntries": JOURNAL_ENTRIES,
        "projectionKeys": PROJECTION_KEYS,
        "hostEvents": HOST_EVENTS,
        "streamKinds": STREAM_KINDS,
        "streamFrames": STREAM_FRAMES,
        "streamEndReasons": STREAM_END_REASONS,
        "clientCalls": CLIENT_CALLS,
        "clientNotes": CLIENT_NOTES,
        "serverCalls": SERVER_CALLS,
        "serverNotes": SERVER_NOTES,
    });
    commit_fixture("surface-tags.json", &value);
    let _back: serde_json::Value = read_back("surface-tags.json");
}

#[test]
fn export_protocol_frame_fixtures() {
    let mut frames = frame_samples(); // snapshot, entry, projections
    let snapshot = frames.remove(0);
    let entry = frames.remove(0);
    let projections = frames.remove(0);
    commit_fixture(
        "frames-snapshot.json",
        &serde_json::to_value(&snapshot).unwrap(),
    );
    commit_fixture("frames-entry.json", &serde_json::to_value(&entry).unwrap());
    commit_fixture(
        "frames-projections.json",
        &serde_json::to_value(&projections).unwrap(),
    );
    commit_fixture(
        "frames-stream-end.json",
        &serde_json::to_value(StreamEndReason::Closed).unwrap(),
    );
    commit_fixture(
        "frames-stream-id.json",
        &serde_json::json!({ "streamId": "stream-1" }),
    );
    commit_fixture(
        "frames-thread-header.json",
        &serde_json::to_value(header_sample()).unwrap(),
    );

    // One §C.1 entry envelope per declared journal event: dense seq, event
    // fields flattened inline (`type` sits next to `seq`).
    let entries: Vec<serde_json::Value> = journal_samples()
        .into_iter()
        .enumerate()
        .map(|(seq, event)| {
            let entry = JournalWireEntry {
                seq: seq as u64,
                id: format!("e-{seq}"),
                parent_id: seq.checked_sub(1).map(|p| format!("e-{p}")),
                timestamp: "2026-09-04T00:00:00Z".into(),
                event,
            };
            serde_json::to_value(&entry).unwrap()
        })
        .collect();
    commit_fixture("journal-entries.json", &serde_json::json!(entries));

    // One host event per declared §D.5 arm.
    let host: Vec<serde_json::Value> = host_samples()
        .iter()
        .map(|h| serde_json::to_value(h).unwrap())
        .collect();
    commit_fixture("host-events.json", &serde_json::json!(host));

    // Round-trip everything we just wrote.
    let snap: StreamFrame = read_back("frames-snapshot.json");
    assert!(matches!(snap, StreamFrame::Snapshot(_)));
    let e: StreamFrame = read_back("frames-entry.json");
    assert!(matches!(e, StreamFrame::Entry { .. }));
    let p: StreamFrame = read_back("frames-projections.json");
    assert!(matches!(p, StreamFrame::Projections(_)));
    let end: StreamEndReason = read_back("frames-stream-end.json");
    assert_eq!(end, StreamEndReason::Closed);
    let entries: Vec<JournalWireEntry> = read_back("journal-entries.json");
    assert_eq!(
        entries.len(),
        manox_protocol::surface::JOURNAL_ENTRIES.len()
    );
    let events: Vec<manox_protocol::stream::HostEvent> = read_back("host-events.json");
    assert_eq!(events.len(), manox_protocol::surface::HOST_EVENTS.len());
}
