//! v2 streaming frames and host events — architecture doc §D.1 (payload
//! half) and §D.5.
//!
//! Declaring surfaces: `FRAMES` (frame vocabulary) and `HOST_EVENTS`
//! (global host notification vocabulary) — see [`crate::surface`].
//!
//! These are the v2 frame *payload* types. The §D.1 envelope variants that
//! carry them (`FromClient::StreamOpen/StreamCancel`,
//! `FromServer::StreamItem/StreamEnd`, `FromServer::Notification { host }`)
//! cannot extend the live v1 enums without breaking the exhaustive matches in
//! `manox-session-core` / `agent-ui` (see the T2 delivery report); the T4/T5
//! envelope migration consumes these types. The backpressure policy below is
//! the §D.7 strategy expressed over the frame vocabulary, ready for that
//! wiring.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::journal::{JournalWireEntry, ThreadHeader};
use crate::server::ServerNote;
use crate::transport::BackpressurePolicy;
use crate::wire::{ModelInfo, ThreadListItem};

/// Entry-stream queue bound (§D.7): when a follow stream's `Entry` queue is
/// full the server must not grow a replay buffer (L5) — it ends the stream
/// with [`StreamEndReason::Resync`] and the client re-follows from a fresh
/// snapshot.
pub const ENTRY_BACKPRESSURE_CAPACITY: usize = 4096;

/// What a client asks a stream to carry (§D.1 `StreamOpen.kind`).
///
/// Declaring surface: FRAMES.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum StreamKind {
    /// Follow one session's journal: snapshot first, then live entries +
    /// projection deltas. `max_messages` bounds the initial window.
    FollowSession {
        session_id: String,
        max_messages: Option<u32>,
    },
}

/// One item delivered inside `FromServer::StreamItem { stream_id, frame }`
/// (§D.1).
///
/// Declaring surface: FRAMES.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum StreamFrame {
    /// Authoritative window opening a stream (L5: never dropped; the
    /// client's journal engine requires it as frame #1, §F.1).
    Snapshot(SessionSnapshot),
    /// One durable journal entry appended at `seq` (L3/L4). Bounded queue:
    /// overflow resyncs instead of dropping (L5).
    Entry {
        seq: u64,
        event: crate::journal::JournalWireEvent,
    },
    /// Changed projection values since the previous frame (P face, §E.1);
    /// never dropped.
    Projections(ProjectionsFrame),
}

impl StreamFrame {
    /// §D.7 overflow policy for this frame class:
    /// - `Snapshot` / `Projections`: never dropped — resync or block;
    /// - `Entry`: bounded queue of [`ENTRY_BACKPRESSURE_CAPACITY`]; when full
    ///   the transport must emit `StreamEnd { reason: Resync }` (the client
    ///   re-follows). A silently dropped entry would open a journal gap the
    ///   client cannot close (L5 — hence "bounded ⇒ resync", not "drop").
    pub fn backpressure_policy(&self) -> BackpressurePolicy {
        match self {
            StreamFrame::Snapshot(_) | StreamFrame::Projections(_) => BackpressurePolicy::NeverDrop,
            StreamFrame::Entry { .. } => BackpressurePolicy::BoundedResync,
        }
    }
}

/// The v1 → v2 backpressure bridge (T2 stop-rule compromise): the live
/// `FromServer::Notification` payload is still [`ServerNote`] (the envelope
/// cannot swap to §D.5's `HostEvent` without breaking the 60 consumer sites;
/// see delivery report). §D.7's "control frames block, never drop" rule is
/// applied to the *whole* notification stream for now — the legacy
/// drop-tolerant classes (`AgentText`/`Thinking`/`ToolOutput`/thread
/// history/bare-model) are exactly the §D.6 doomed set: their durable
/// successors are `StreamFrame::Entry` deltas (bounded ⇒ resync, L5), so
/// once streams exist they must not silently drop either. Wiring this policy
/// into `InProcessConnection::send_to_client` is the T4/T5 task.
pub fn v2_backpressure_policy(note: &ServerNote) -> BackpressurePolicy {
    let _ = note;
    BackpressurePolicy::Disconnect
}

/// Opening snapshot of a follow stream (§D.1): window records (dense seq),
/// the thread header, the journal cursor, and the full projection baseline.
///
/// Declaring surface: FRAMES (payload of `StreamFrame::Snapshot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub session_id: String,
    /// Journal header (line 0) of the thread.
    pub header: ThreadHeader,
    /// Cursor = number of active-chain entries; the snapshot window ends
    /// exactly at `cursor` (§F.1 rule 1).
    pub cursor: u64,
    /// Window records, dense seq, oldest first.
    pub records: Vec<JournalWireEntry>,
    /// Older records exist before the window (truncated by `max_messages`).
    pub has_more: bool,
    /// Full projection baseline, key → value (§E.2 key table).
    pub projections: BTreeMap<String, serde_json::Value>,
    /// `seq` the baseline was folded at.
    pub projections_as_of_seq: u64,
}

/// Delta of changed projection keys since the previous frame (§D.1, §E.1):
/// only changed entries are carried; clients keep higher-`as_of_seq`-wins.
///
/// Declaring surface: FRAMES (payload of `StreamFrame::Projections`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "camelCase")]
pub struct ProjectionsFrame {
    pub session_id: String,
    /// Triggering entry's seq (the fold point, §E.1).
    pub as_of_seq: u64,
    /// Changed keys only: key → value.
    pub values: BTreeMap<String, serde_json::Value>,
}

/// Why a stream ended (§D.1). `Resync` is the L5 overflow signal: the client
/// must re-follow (fresh snapshot), never replay from a server buffer.
///
/// Declaring surface: FRAMES (payload of `StreamEnd`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum StreamEndReason {
    /// Server closed the stream cleanly (session disposed, ownership lost).
    Closed,
    /// Client cancelled (`StreamCancel`).
    Cancelled,
    /// Entry queue overflowed the bounded (§D.7) window: client must
    /// re-follow from a fresh snapshot (L5).
    Resync,
    /// Server-side failure with a stable code (§D.7 code set).
    Failure { code: String, message: String },
}

/// Global host notification — the §D.5 vocabulary replacing the doomed
/// server-side domain notes (`ServerNote::Models`/`Commands`/
/// `ThreadsUpdated`/turn-lifecycle/`Error`/session-control, §D.6). Carried by
/// the v2 envelope's `FromServer::Notification { host }` (T4/T5 wiring); on
/// the v1 wire the same shapes are still delivered via `ServerNote` (L12:
/// unknown variants are dropped + logged, never disconnecting).
///
/// Declaring surface: HOST_EVENTS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HostEvent {
    /// Handshake ack: the server's protocol epoch (L12: `Initialize` carries
    /// the epoch; `Ready` echoes the accepted one).
    Ready { epoch: u32 },
    /// Model registry snapshot; pushed whenever providers reload (§D.5).
    Models { models: Vec<ModelInfo> },
    /// Slash-command / skill list snapshot.
    Commands { commands: serde_json::Value },
    /// Threads-list metadata changed: full snapshot (title/pin/archive/
    /// model/grouping), not a delta (§D.5).
    ThreadsUpdated { threads: Vec<ThreadListItem> },
    /// Per-session status mirror — the small high-frequency delta replacing
    /// the doomed turn-lifecycle notes. Broadcast to *all* connections; the
    /// client applies the monotonic mirror rules of §D.5 (unread only
    /// increases until focus, errored is an edge flag, running is latest).
    SessionStatus {
        session_id: String,
        running: Option<bool>,
        errored: Option<bool>,
        unread: Option<bool>,
        pending_auth: Option<bool>,
        pending_plan: Option<bool>,
        background_work: Option<bool>,
    },
    /// A session was created (owner-set control, carries the header).
    SessionCreated {
        session_id: String,
        header: ThreadHeader,
    },
    /// A session was disposed (owner-set control).
    SessionDisposed { session_id: String },
    /// A host-level error.
    Error { message: String },
}

impl HostEvent {
    /// §D.7 policy for host-event traffic: control / lifecycle class — the
    /// queue blocks, nothing is ever silently dropped.
    pub fn backpressure_policy(&self) -> BackpressurePolicy {
        BackpressurePolicy::Disconnect
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::JournalWireEvent;

    fn header() -> ThreadHeader {
        ThreadHeader {
            id: "s1".into(),
            cwd: "/proj".into(),
            parent_session: None,
            metadata: None,
            created_at: "2026-09-04T00:00:00Z".into(),
        }
    }

    #[test]
    fn snapshot_round_trips_with_projections_and_records() {
        let snap = SessionSnapshot {
            session_id: "s1".into(),
            header: header(),
            cursor: 1,
            records: vec![JournalWireEntry {
                seq: 0,
                id: "e0".into(),
                parent_id: None,
                timestamp: "2026-09-04T00:00:00Z".into(),
                event: JournalWireEvent::TurnStart,
            }],
            has_more: false,
            projections: BTreeMap::from([
                ("title".into(), serde_json::json!("hello")),
                ("running".into(), serde_json::json!(true)),
            ]),
            projections_as_of_seq: 0,
        };
        let frame = StreamFrame::Snapshot(snap);
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["type"], "snapshot");
        assert_eq!(json["header"]["createdAt"], "2026-09-04T00:00:00Z");
        assert_eq!(json["projections"]["running"], true);
        assert_eq!(json["records"][0]["type"], "turnStart");
        let back: StreamFrame = serde_json::from_value(json).unwrap();
        assert_eq!(frame, back);
    }

    #[test]
    fn entry_and_projection_frames_round_trip() {
        let frame = StreamFrame::Entry {
            seq: 3,
            event: JournalWireEvent::AgentTextDelta { s: "tok".into() },
        };
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["type"], "entry");
        assert_eq!(json["seq"], 3);
        assert_eq!(json["event"]["type"], "agentTextDelta");
        let back: StreamFrame = serde_json::from_value(json).unwrap();
        assert_eq!(frame, back);

        let frame = StreamFrame::Projections(ProjectionsFrame {
            session_id: "s1".into(),
            as_of_seq: 9,
            values: BTreeMap::from([(
                "model".into(),
                serde_json::json!({"provider": "p", "id": "m"}),
            )]),
        });
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["type"], "projections");
        assert_eq!(json["asOfSeq"], 9);
        let back: StreamFrame = serde_json::from_value(json).unwrap();
        assert_eq!(frame, back);
    }

    #[test]
    fn frame_backpressure_classes_follow_section_d7() {
        assert_eq!(
            StreamFrame::Snapshot(SessionSnapshot {
                session_id: "s".into(),
                header: header(),
                cursor: 0,
                records: vec![],
                has_more: false,
                projections: BTreeMap::new(),
                projections_as_of_seq: 0,
            })
            .backpressure_policy(),
            BackpressurePolicy::NeverDrop
        );
        assert_eq!(
            StreamFrame::Entry {
                seq: 1,
                event: JournalWireEvent::Stop { reason: None },
            }
            .backpressure_policy(),
            BackpressurePolicy::BoundedResync
        );
        assert_eq!(
            StreamFrame::Projections(ProjectionsFrame {
                session_id: "s".into(),
                as_of_seq: 0,
                values: BTreeMap::new(),
            })
            .backpressure_policy(),
            BackpressurePolicy::NeverDrop
        );
    }

    #[test]
    fn host_events_round_trip() {
        let evs = vec![
            HostEvent::Ready { epoch: 2 },
            HostEvent::Models { models: vec![] },
            HostEvent::Commands {
                commands: serde_json::json!([]),
            },
            HostEvent::ThreadsUpdated { threads: vec![] },
            HostEvent::SessionStatus {
                session_id: "s1".into(),
                running: Some(true),
                errored: None,
                unread: Some(false),
                pending_auth: None,
                pending_plan: None,
                background_work: Some(true),
            },
            HostEvent::SessionCreated {
                session_id: "s1".into(),
                header: header(),
            },
            HostEvent::SessionDisposed {
                session_id: "s1".into(),
            },
            HostEvent::Error {
                message: "boom".into(),
            },
        ];
        for ev in &evs {
            let json = serde_json::to_string(ev).unwrap();
            let back: HostEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(*ev, back, "HostEvent round-trip failed: {json}");
        }
        let json = serde_json::to_value(&evs[0]).unwrap();
        assert_eq!(json["type"], "ready");
        assert_eq!(json["epoch"], 2);
    }

    #[test]
    fn stream_end_reasons_are_typed_and_tagged() {
        for reason in [
            StreamEndReason::Closed,
            StreamEndReason::Cancelled,
            StreamEndReason::Resync,
            StreamEndReason::Failure {
                code: "gateway/internal".into(),
                message: "oops".into(),
            },
        ] {
            let json = serde_json::to_value(&reason).unwrap();
            let back: StreamEndReason = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(reason, back);
            assert!(json["type"].is_string());
        }
        assert_eq!(
            serde_json::to_value(StreamEndReason::Resync).unwrap()["type"],
            "resync"
        );
    }

    #[test]
    fn stream_kind_follow_session_round_trips() {
        let kind = StreamKind::FollowSession {
            session_id: "s1".into(),
            max_messages: Some(50),
        };
        let json = serde_json::to_value(&kind).unwrap();
        assert_eq!(json["type"], "followSession");
        assert_eq!(json["sessionId"], "s1");
        assert_eq!(json["maxMessages"], 50);
        let back: StreamKind = serde_json::from_value(json).unwrap();
        assert_eq!(kind, back);
    }
}
