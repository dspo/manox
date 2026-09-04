//! Fixture export (§D.8): real frames as JSON files consumed by the webui
//! vitest guard suite (T3) — dual-path consistency on the TS side (J.5).
//!
//! Every run of the test suite rewrites `crates/manox-protocol/fixtures/`
//! from the same typed samples the surface harness walks, so fixtures can
//! never drift from the Rust serde shape. The files cover the four stream
//! item classes (`Snapshot` / `Entry` / `Projections` / `StreamEnd`), every
//! journal event type, and every host event.

use std::path::PathBuf;

use manox_protocol::journal::JournalWireEntry;
use manox_protocol::stream::{StreamEndReason, StreamFrame};
use manox_protocol::surface::{frame_samples, header_sample, host_samples, journal_samples};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn write(name: &str, value: &serde_json::Value) {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).expect("create fixtures dir");
    std::fs::write(
        dir.join(name),
        format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
    )
    .expect("write fixture");
}

fn read_back<T: serde::de::DeserializeOwned>(name: &str) -> T {
    let path = fixtures_dir().join(name);
    let text = std::fs::read_to_string(&path).expect("read fixture");
    serde_json::from_str(&text).expect("fixture parses")
}

#[test]
fn export_protocol_frame_fixtures() {
    let mut frames = frame_samples(); // snapshot, entry, projections
    let snapshot = frames.remove(0);
    let entry = frames.remove(0);
    let projections = frames.remove(0);
    write(
        "frames-snapshot.json",
        &serde_json::to_value(&snapshot).unwrap(),
    );
    write("frames-entry.json", &serde_json::to_value(&entry).unwrap());
    write(
        "frames-projections.json",
        &serde_json::to_value(&projections).unwrap(),
    );
    write(
        "frames-stream-end.json",
        &serde_json::to_value(StreamEndReason::Closed).unwrap(),
    );
    write(
        "frames-stream-id.json",
        &serde_json::json!({ "streamId": "stream-1" }),
    );
    write(
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
    write("journal-entries.json", &serde_json::json!(entries));

    // One host event per declared §D.5 arm.
    let host: Vec<serde_json::Value> = host_samples()
        .iter()
        .map(|h| serde_json::to_value(h).unwrap())
        .collect();
    write("host-events.json", &serde_json::json!(host));

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
