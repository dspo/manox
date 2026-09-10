//! T2 declaration-surface coverage harness (§J.4 / J.5, L12), C3 revision.
//!
//! Builds a scripted conversation (§D.1 follow stream: snapshot → journal
//! entry per declared `JournalWireEvent` → projections delta → stream end)
//! plus the scripted `HostEvent` sequence on the REAL `FromServer::Host`
//! envelope, and asserts that every name in every declaration table of
//! [`manox_protocol::surface`] (a) constructs, (b) round-trips through
//! serde, (c) serializes with exactly the declared tag — cross-checked
//! against the surface's exhaustive tag match — and (d) appears inside some
//! scripted frame.
//!
//! The tables, tag matches, and samples are generated from one
//! `wire_surface!` list per enum (see the surface module docs): a new enum
//! variant is a compile error until it is declared AND sampled, so this
//! harness can no longer "pass" while a variant is invisible — the former
//! self-referential gap (hand-written tables checked against hand-written
//! samples, e.g. `FRAMES` lacking `followSession`, host events riding a
//! fake `Response` envelope while production used `FromServer::Host`).

use manox_protocol::journal::JournalWireEntry;
use manox_protocol::stream::{HostEvent, StreamEndReason, StreamFrame, StreamKind};
use manox_protocol::surface::{
    CLIENT_CALLS, CLIENT_NOTES, HOST_EVENTS, JOURNAL_ENTRIES, PROJECTION_KEYS, SERVER_CALLS,
    SERVER_NOTES, STREAM_END_REASONS, STREAM_FRAMES, STREAM_KINDS, client_call_samples,
    client_call_tag, client_note_samples, client_note_tag, frame_samples, frames, host_samples,
    host_wire_tag, journal_samples, journal_wire_tag, scripted_host_events, scripted_session,
    scripted_stream_open, server_call_samples, server_call_tag, server_note_samples,
    server_note_tag, stream_end_samples, stream_end_tag, stream_frame_tag, stream_kind_samples,
    stream_kind_tag,
};
use manox_protocol::{ClientCall, ClientNote, RpcError, ServerCall, ServerNote};

/// Zip a generated table with its generated samples into `(name, value)`
/// pairs for [`walk`]. Lengths are equal by construction (one macro list
/// generates both); the assert documents that invariant.
fn pairs<T>(table: &[&str], samples: Vec<T>) -> Vec<(String, T)> {
    assert_eq!(
        table.len(),
        samples.len(),
        "table/sample length mismatch — both come from one wire_surface! list"
    );
    table
        .iter()
        .zip(samples)
        .map(|(name, value)| (name.to_string(), value))
        .collect()
}

// ── generic walk: construct + declared tag + serde round-trip ─────────────

fn walk<T: PartialEq + std::fmt::Debug>(
    table: &[&str],
    tag_field: &str,
    samples: Vec<(String, T)>,
    ser: impl Fn(&T) -> serde_json::Value,
    de: impl Fn(serde_json::Value) -> T,
) {
    let names: Vec<&str> = samples.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names, table,
        "sample set drifted from the declared table (field {tag_field:?})"
    );
    for (name, value) in &samples {
        let json = ser(value);
        assert_eq!(
            json[tag_field],
            serde_json::Value::String(name.clone()),
            "wire tag mismatch for {name}: {json}"
        );
        let back = de(json);
        assert_eq!(value, &back, "round-trip failed for {name}");
    }
}

/// Derive the wire tag of an internally tagged value by reading the field.
fn tag_of(value: &serde_json::Value, field: &str) -> String {
    value[field].as_str().expect("string tag").to_string()
}

#[test]
fn journal_entries_surface_is_complete() {
    let samples = journal_samples();
    // The exhaustive tag match agrees with serde on every sample (the match
    // is the compile-time gate; this pins it to the wire representation).
    for (tag, sample) in JOURNAL_ENTRIES.iter().zip(samples.iter()) {
        assert_eq!(journal_wire_tag(sample), *tag);
    }
    let pairs: Vec<(String, _)> = samples
        .iter()
        .cloned()
        .map(|e| {
            let v = serde_json::to_value(&e).unwrap();
            (tag_of(&v, "type"), e)
        })
        .collect();
    walk(
        JOURNAL_ENTRIES,
        "type",
        pairs,
        |e| serde_json::to_value(e).unwrap(),
        |v| serde_json::from_value(v).unwrap(),
    );
    // The §C.1 entry envelope also round-trips every declared event.
    for (seq, event) in journal_samples().into_iter().enumerate() {
        let entry = JournalWireEntry {
            seq: seq as u64,
            id: format!("e-{seq}"),
            parent_id: Some(format!("e-{}", seq.saturating_sub(1))),
            timestamp: "2026-09-04T00:00:00Z".into(),
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: JournalWireEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back, "entry envelope round-trip failed: {json}");
    }
}

#[test]
fn host_events_surface_is_complete() {
    let samples = host_samples();
    for (tag, sample) in HOST_EVENTS.iter().zip(samples.iter()) {
        assert_eq!(host_wire_tag(sample), *tag);
    }
    let pairs: Vec<(String, _)> = samples
        .iter()
        .cloned()
        .map(|h| {
            let v = serde_json::to_value(&h).unwrap();
            (tag_of(&v, "type"), h)
        })
        .collect();
    walk(
        HOST_EVENTS,
        "type",
        pairs,
        |h| serde_json::to_value(h).unwrap(),
        |v| serde_json::from_value::<HostEvent>(v).unwrap(),
    );
}

#[test]
fn frames_surface_is_complete() {
    for (tag, sample) in STREAM_KINDS.iter().zip(stream_kind_samples().iter()) {
        assert_eq!(stream_kind_tag(sample), *tag);
    }
    walk(
        STREAM_KINDS,
        "type",
        pairs(STREAM_KINDS, stream_kind_samples()),
        |k| serde_json::to_value(k).unwrap(),
        |v| serde_json::from_value::<StreamKind>(v).unwrap(),
    );
    for (tag, sample) in STREAM_FRAMES.iter().zip(frame_samples().iter()) {
        assert_eq!(stream_frame_tag(sample), *tag);
    }
    walk(
        STREAM_FRAMES,
        "type",
        pairs(STREAM_FRAMES, frame_samples()),
        |f| serde_json::to_value(f).unwrap(),
        |v| serde_json::from_value::<StreamFrame>(v).unwrap(),
    );
    for (tag, sample) in STREAM_END_REASONS.iter().zip(stream_end_samples().iter()) {
        assert_eq!(stream_end_tag(sample), *tag);
    }
    walk(
        STREAM_END_REASONS,
        "type",
        pairs(STREAM_END_REASONS, stream_end_samples()),
        |r| serde_json::to_value(r).unwrap(),
        |v| serde_json::from_value::<StreamEndReason>(v).unwrap(),
    );
    // `frames()` is the §D.1 vocabulary the spec names: the mechanical
    // concatenation of the three generated tables.
    let mut want = STREAM_KINDS.to_vec();
    want.extend_from_slice(STREAM_FRAMES);
    want.extend_from_slice(STREAM_END_REASONS);
    assert_eq!(frames(), want);
}

#[test]
fn current_call_surface_is_complete() {
    for (tag, sample) in CLIENT_CALLS.iter().zip(client_call_samples().iter()) {
        assert_eq!(client_call_tag(sample), *tag);
    }
    walk(
        CLIENT_CALLS,
        "method",
        pairs(CLIENT_CALLS, client_call_samples()),
        |c| serde_json::to_value(c).unwrap(),
        |v| serde_json::from_value::<ClientCall>(v).unwrap(),
    );
    for (tag, sample) in CLIENT_NOTES.iter().zip(client_note_samples().iter()) {
        assert_eq!(client_note_tag(sample), *tag);
    }
    walk(
        CLIENT_NOTES,
        "method",
        pairs(CLIENT_NOTES, client_note_samples()),
        |c| serde_json::to_value(c).unwrap(),
        |v| serde_json::from_value::<ClientNote>(v).unwrap(),
    );
    for (tag, sample) in SERVER_CALLS.iter().zip(server_call_samples().iter()) {
        assert_eq!(server_call_tag(sample), *tag);
    }
    walk(
        SERVER_CALLS,
        "method",
        pairs(SERVER_CALLS, server_call_samples()),
        |c| serde_json::to_value(c).unwrap(),
        |v| serde_json::from_value::<ServerCall>(v).unwrap(),
    );
    for (tag, sample) in SERVER_NOTES.iter().zip(server_note_samples().iter()) {
        assert_eq!(server_note_tag(sample), *tag);
    }
    walk(
        SERVER_NOTES,
        "method",
        pairs(SERVER_NOTES, server_note_samples()),
        |c| serde_json::to_value(c).unwrap(),
        |v| serde_json::from_value::<ServerNote>(v).unwrap(),
    );
}

/// §J.4 (d): every declared name appears, serialized, inside some scripted
/// frame — on the real production envelopes (`FromServer::StreamItem` /
/// `StreamEnd` / `Host`, `FromClient::StreamOpen`). This is still a
/// script-side proof: the J1b gate (real-composition emission coverage)
/// drives an actual server; this harness guarantees the vocabulary itself
/// is representable and represented.
#[test]
fn every_declared_surface_name_appears_in_scripted_frames() {
    let session: Vec<String> = scripted_session()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    let host: Vec<String> = scripted_host_events()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    let open = serde_json::to_string(&scripted_stream_open()).unwrap();

    // The scripted host frames ride the real FromServer::Host envelope.
    assert!(
        host.iter().all(|s| s.contains("\"kind\":\"host\"")),
        "scripted host events must ride the production Host envelope: {host:?}"
    );

    for name in JOURNAL_ENTRIES {
        assert!(
            session
                .iter()
                .any(|s| s.contains(&format!("\"type\":\"{name}\""))),
            "journal entry {name} never emitted in the scripted session"
        );
    }
    for name in HOST_EVENTS {
        assert!(
            host.iter()
                .any(|s| s.contains(&format!("\"type\":\"{name}\""))),
            "host event {name} never emitted in the scripted host stream"
        );
    }
    for name in STREAM_FRAMES.iter().chain(STREAM_END_REASONS.iter()) {
        // Key-order-independent containment: serde's internally tagged
        // serialization does not place `type` first (struct-variant fields
        // may sort ahead of the tag — Snapshot and Failure both do), so never
        // needle across a `{"type":…` boundary. Frame names collide with
        // neither journal-entry nor host-event tags.
        let needle = format!("\"type\":\"{name}\"");
        assert!(
            session.iter().any(|s| s.contains(&needle)),
            "frame {name} never emitted in the scripted session"
        );
    }
    for name in STREAM_KINDS {
        let needle = format!("\"type\":\"{name}\"");
        assert!(
            open.contains(&needle),
            "stream kind {name} never emitted in the scripted StreamOpen"
        );
    }
    assert!(
        session
            .iter()
            .any(|s| s.contains("\"streamId\":\"stream-1\"")),
        "stream_id is part of the §D.1 frame shape"
    );
}

/// L5 / §D.7: the frame policy classes and the §D.7 error code set are
/// asserted through the public API (backpressure harness).
#[test]
fn backpressure_classes_and_resync_plumbing() {
    for f in frame_samples() {
        match &f {
            StreamFrame::Entry { .. } => {
                assert_eq!(
                    f.backpressure_policy(),
                    manox_protocol::transport::BackpressurePolicy::BoundedResync
                );
            }
            _ => {
                assert_eq!(
                    f.backpressure_policy(),
                    manox_protocol::transport::BackpressurePolicy::NeverDrop
                );
            }
        }
    }
    // The resync signal is expressible as a stream end + an error code.
    let reason = StreamEndReason::Resync;
    let err =
        RpcError::new(1, "queue overflow").with_code(manox_protocol::msg::CODE_RESYNC_REQUIRED);
    assert_eq!(err.data.as_ref().unwrap()["code"], "resync-required");
    assert_eq!(
        serde_json::to_value(&reason).unwrap()["type"],
        "resync",
        "resync tag drift"
    );
}

/// §E.2: the snapshot baseline carries exactly the 20 declared projection
/// keys (the scripted conversation's snapshot is the client-visible proof).
#[test]
fn snapshot_projects_every_declared_projection_key() {
    let snap = frame_samples()
        .into_iter()
        .find_map(|f| match f {
            StreamFrame::Snapshot(s) => Some(s),
            _ => None,
        })
        .expect("scripted session opens with a snapshot");
    for key in PROJECTION_KEYS {
        assert!(
            snap.projections.contains_key(*key),
            "projection key {key} missing from the snapshot baseline"
        );
    }
    assert_eq!(snap.projections.len(), PROJECTION_KEYS.len());
}

/// §D.6 (T10 removal pass): a deleted arm must not resurface — an unknown
/// `ServerNote` method is a deserialize error, and the retained-face walk
/// above keeps the table, the samples, and the enum in lockstep.
#[test]
fn doomed_server_notes_are_gone() {
    for name in [
        "agentText",
        "threadHistory",
        "usageSnapshot",
        "steerPending",
    ] {
        assert!(!SERVER_NOTES.contains(&name), "{name} resurfaced");
    }
    let err = serde_json::from_value::<ServerNote>(serde_json::json!({
        "method": "agentText",
        "sessionId": "s1",
        "text": "x",
    }))
    .expect_err("agentText must no longer deserialize");
    assert!(format!("{err}").contains("agentText") || format!("{err}").contains("variant"));
}
