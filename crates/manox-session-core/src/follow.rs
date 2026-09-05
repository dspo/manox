//! Follow streams (§D.1 / §F server side, T4).
//!
//! One task per `FromClient::StreamOpen { stream_kind: FollowSession }`: it
//! subscribes to the thread's journal feed **before** the snapshot read
//! (atomicity: entries landing in between are replayed idempotently by the
//! client's seq algebra, §F.1 rule 2), sends the opening
//! [`StreamFrame::Snapshot`], then forwards live appends as
//! [`StreamFrame::Entry`] frames.
//!
//! Backpressure (§D.7 / L5): the Entry window is bounded — the feed's
//! broadcast queue, capacity
//! [`manox_protocol::ENTRY_BACKPRESSURE_CAPACITY`] — and on overflow the
//! stream ends with `StreamEnd { Resync }`; snapshot / projections / stream
//! end frames never drop (they ride the connection's non-dropping control
//! path). The kernel feed itself surfaces a lagging subscriber as
//! [`JournalFeed::Lagged`], which is the server-side twin of the same rule:
//! resync, never silent loss.
//!
//! The projection baseline is empty with `as_of_seq = cursor` (§D.1 T4
//! scope): the projection registry is T5 and folds no keys yet, so the
//! snapshot carries `projections: {}` and `StreamFrame::Projections` delta
//! frames are only produced by T5's pump.

use std::sync::{Arc, Mutex as StdMutex};

use manox_agent::engine::JournalFeed;
use manox_agent::thread::ThreadHandle;
use manox_protocol::journal::StreamId;
use manox_protocol::stream::{SessionSnapshot, StreamEndReason, StreamFrame};
use manox_protocol::{FromServer, RpcConnection};
use tokio_util::sync::CancellationToken;

use crate::translate::wire_entry;

/// The control side of a live follow stream, held by `AgentServerInner`.
/// [`StreamHandle::end`] requests a terminal reason; the stream task sends
/// exactly one `FromServer::StreamEnd` (L5: never dropped) and unregisters
/// itself.
#[derive(Clone)]
pub struct StreamHandle {
    session_id: String,
    cancel: CancellationToken,
    reason: Arc<StdMutex<Option<StreamEndReason>>>,
}

impl StreamHandle {
    /// Mint a control handle from the caller-owned cancel token + reason
    /// cell (the dispatch side holds them so the unregister-after-end
    /// closure can identity-guard the registry entry).
    pub(crate) fn new(
        session_id: String,
        cancel: CancellationToken,
        reason: Arc<StdMutex<Option<StreamEndReason>>>,
    ) -> Self {
        Self {
            session_id,
            cancel,
            reason,
        }
    }

    /// The control inputs [`spawn_follow_stream`] consumes (moved).
    pub(crate) fn parts(&self) -> (CancellationToken, Arc<StdMutex<Option<StreamEndReason>>>) {
        (self.cancel.clone(), Arc::clone(&self.reason))
    }

    /// The session this stream follows (registry lookup on dispose).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Identity against a clone of the same handle (used by the
    /// unregister-after-end path to avoid deleting a superseded entry).
    pub fn is_same_handle(&self, other: &StreamHandle) -> bool {
        Arc::ptr_eq(&self.reason, &other.reason)
    }

    /// Request the stream to end with `reason` (cancelled / disposed).
    /// Idempotent; the first requested reason wins.
    pub fn end(&self, reason: StreamEndReason) {
        let mut slot = self.reason.lock().unwrap();
        if slot.is_none() {
            *slot = Some(reason);
        }
        self.cancel.cancel();
    }
}

/// Spawn the follow task for one `StreamOpen { FollowSession }`. The caller
/// (the per-connection dispatch task) creates the [`StreamHandle`] first
/// (`StreamHandle::new`), tracks it, and passes `on_end` — invoked once with
/// the terminal reason after the `StreamEnd` frame is sent, where the caller
/// unregisters (identity-guarded).
pub fn spawn_follow_stream(
    conn: Arc<dyn RpcConnection>,
    stream_id: StreamId,
    session_id: String,
    max_messages: Option<u32>,
    thread: ThreadHandle,
    handle: &StreamHandle,
    on_end: impl FnOnce(StreamEndReason) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let (cancel, reason) = handle.parts();
    manox_agent::runtime::handle().spawn(async move {
        let end = run_follow_stream(
            conn,
            stream_id,
            session_id,
            max_messages,
            thread,
            cancel,
            reason,
        )
        .await;
        on_end(end);
    })
}

/// Take the requested end reason (set by [`StreamHandle::end`]), if any.
fn requested_reason(reason: &Arc<StdMutex<Option<StreamEndReason>>>) -> Option<StreamEndReason> {
    reason.lock().unwrap().clone()
}

async fn run_follow_stream(
    conn: Arc<dyn RpcConnection>,
    stream_id: StreamId,
    session_id: String,
    max_messages: Option<u32>,
    thread: ThreadHandle,
    cancel: CancellationToken,
    reason: Arc<StdMutex<Option<StreamEndReason>>>,
) -> StreamEndReason {
    // ── Atomicity: subscribe BEFORE the snapshot read. ─────────────────────
    let mut feed = thread.subscribe_journal_feed();
    // ── Snapshot (whole active chain via the kernel read seam, §C.3). ──────
    //
    // The seam answers from the engine actor's command loop. While the
    // engine is still booting (or has been replaced by `open_session`) the
    // oneshot reply can be dropped without an answer, which surfaces as an
    // `Err` -> `None` from `journal_snapshot()`. Treat that as "not materialized yet" and
    // retry with a bounded backoff; a genuinely absent engine (landing
    // thread) is answered with `Failure`.
    let end = match snapshot_with_retry(&thread, &cancel).await {
        SnapshotResult::Data(data) => {
            let mut records: Vec<manox_protocol::journal::JournalWireEntry> =
                Vec::with_capacity(data.records.len());
            // The per-stream projection set (P face, §E): seeded from the
            // live thread, folded forward by every record — including the
            // wire-projection-less ones (folds are kernel-level). After the
            // loop the baseline is consistent with the snapshot cursor.
            let mut projections = crate::projections::ProjectionSet::seed(&thread);
            for record in &data.records {
                projections.apply_event(record.seq, &record.entry);
                // The §C.2 projection is total (no wire-less kinds), so the
                // page is seq-dense end-to-end — the fold's `assertPage`
                // adjacency holds.
                if let Some(entry) = wire_entry(record.seq, &record.entry) {
                    records.push(entry);
                }
            }
            // Header `createdAt` fallback: the oldest wire-mapped record
            // (§C.3 seam carries no file header — T4 gap note).
            let oldest = records.first().map(|r| r.timestamp.clone());
            // `max_messages` bounds the initial window from the tail.
            let (window, has_more) = match max_messages {
                Some(n) if (records.len() as u32) > n => {
                    let start = records.len() - n as usize;
                    (records.split_off(start), true)
                }
                _ => (records, false),
            };
            let snapshot = SessionSnapshot {
                session_id: session_id.clone(),
                header: snapshot_header(&thread, &session_id, oldest),
                cursor: data.cursor,
                records: window,
                has_more,
                // T5 projection registry: no keys folded yet — empty
                // baseline, as of the read cursor (§D.1).
                projections: projections.baseline(),
                projections_as_of_seq: data.cursor,
            };
            conn.send_to_client(FromServer::StreamItem {
                stream_id: stream_id.clone(),
                frame: StreamFrame::Snapshot(snapshot),
            });
            forward_entries(
                &conn,
                &stream_id,
                &session_id,
                &mut feed,
                &mut projections,
                &cancel,
                &reason,
            )
            .await
        }
        SnapshotResult::Unavailable => {
            requested_reason(&reason).unwrap_or(StreamEndReason::Failure {
                code: manox_protocol::msg::CODE_GATEWAY_INTERNAL.into(),
                message: "journal engine is not materialized".into(),
            })
        }
    };
    finish(&conn, &stream_id, end)
}

/// Outcome of the opening snapshot read (§C.3 seam).
enum SnapshotResult {
    Data(manox_agent::engine::JournalSnapshotData),
    Unavailable,
}

/// Read the whole active chain with a bounded retry window: the seam answers
/// from the engine actor's command loop, so a reply dropped during engine
/// boot / session replacement is retried (~30s at 100ms — a materializing
/// PiEngine answers promptly; a never-materializing landing thread gets the
/// `Failure` terminal frame).
async fn snapshot_with_retry(thread: &ThreadHandle, cancel: &CancellationToken) -> SnapshotResult {
    for _ in 0..300u32 {
        if cancel.is_cancelled() {
            return SnapshotResult::Unavailable;
        }
        match thread.journal_snapshot().await {
            Some(data) => return SnapshotResult::Data(data),
            None => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    SnapshotResult::Unavailable
}

/// Forward live feed events as Entry frames until cancelled / Lagged /
/// channel close. Returns the [`StreamEndReason`] to terminate with.
#[allow(clippy::too_many_arguments)] // stream plumbing: each input is distinct
async fn forward_entries(
    conn: &Arc<dyn RpcConnection>,
    stream_id: &StreamId,
    session_id: &str,
    feed: &mut tokio::sync::broadcast::Receiver<JournalFeed>,
    projections: &mut crate::projections::ProjectionSet,
    cancel: &CancellationToken,
    reason: &Arc<StdMutex<Option<StreamEndReason>>>,
) -> StreamEndReason {
    // §D.7 bounded window: the per-stream Entry queue is the feed's
    // broadcast capacity (the kernel sizes it to exactly
    // [`manox_protocol::ENTRY_BACKPRESSURE_CAPACITY`]). While a client's
    // outbound channel is saturated the blocking send stalls this
    // forwarding, the feed backlog grows, and the kernel surfaces the
    // overflow as [`JournalFeed::Lagged`] — which maps to
    // `StreamEnd { Resync }`: overflow resyncs, it never silently drops
    // (L5). A lag that outruns the broadcast channel is reported the same
    // way through the recv error. Snapshot / StreamEnd frames ride the
    // connection's non-dropping control path.
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                return requested_reason(reason).unwrap_or(StreamEndReason::Closed);
            }
            received = feed.recv() => match received {
                Ok(JournalFeed::Event(event)) => {
                    // §C.2 totality: every journal entry has a wire row, so
                    // every feed event forwards as an Entry frame — the seq
                    // stream stays dense (§F.1 rule 2 is vacuous now).
                    if let Some(frame_event) = crate::translate::wire_event(&event.entry) {
                        conn.send_to_client(FromServer::StreamItem {
                            stream_id: stream_id.clone(),
                            frame: StreamFrame::Entry {
                                seq: event.seq,
                                event: frame_event,
                            },
                        });
                    }
                    // P face: fold server-side, then publish changed keys (§E.1).
                    projections.apply_event(event.seq, &event.entry);
                    if let Some((as_of_seq, values)) = projections.drain_changed() {
                        conn.send_to_client(FromServer::StreamItem {
                            stream_id: stream_id.clone(),
                            frame: StreamFrame::Projections(
                                manox_protocol::stream::ProjectionsFrame {
                                    session_id: session_id.to_string(),
                                    as_of_seq,
                                    values,
                                },
                            ),
                        });
                    }
                }
                Ok(JournalFeed::Lagged(n)) => {
                    tracing::warn!(
                        stream = %stream_id.0, skipped = n,
                        "follow stream: journal feed lagged — resync (§D.7/L5)"
                    );
                    return StreamEndReason::Resync;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        stream = %stream_id.0, skipped = n,
                        "follow stream: journal feed lagged — resync (§D.7/L5)"
                    );
                    return StreamEndReason::Resync;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return requested_reason(reason).unwrap_or(StreamEndReason::Closed);
                }
            }
        }
    }
}

fn finish(
    conn: &Arc<dyn RpcConnection>,
    stream_id: &StreamId,
    end: StreamEndReason,
) -> StreamEndReason {
    conn.send_to_client(FromServer::StreamEnd {
        stream_id: stream_id.clone(),
        reason: end.clone(),
    });
    end
}

/// Assemble the snapshot header (§D.1) from the thread's visible state.
/// The journal file's line-0 header is not carried by the kernel read seam
/// (§C.3), so the header is projected from the live thread: `cwd` is the
/// effective thread cwd, `createdAt` the oldest wire-mapped record's
/// timestamp (kernel header timestamp unavailable — T4 gap note).
fn snapshot_header(
    thread: &ThreadHandle,
    session_id: &str,
    created: Option<String>,
) -> manox_protocol::journal::ThreadHeader {
    let cwd = thread.read(|t| t.cwd().to_string_lossy().into_owned());
    manox_protocol::journal::ThreadHeader {
        id: session_id.to_string(),
        cwd,
        parent_session: None,
        metadata: None,
        created_at: created.unwrap_or_else(|| {
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        }),
    }
}
