//! Journal v4 vocabulary — the durable, on-disk entry set of a session.
//!
//! This crate is the leaf that both the runtime (append + replay + cold read)
//! and the AHP host layer (translation into AHP channel actions) depend on.
//! It deliberately carries no I/O and no gateway knowledge: a session file is
//! `<session>.jsonl`, line 0 is the [`ThreadHeader`], every following line is a
//! [`JournalWireEntry`] carrying one [`JournalWireEvent`] (architecture doc §C).
//!
//! Why a leaf crate rather than a module of the gateway: the AHP host
//! (`manox-ahp`) translates entries into AHP actions while the gateway
//! (`manox-session-core`) implements that host's `Backend` seam — two crates
//! that would form a dependency cycle if the vocabulary lived in either one.
//! During the v2→v3 migration window `manox-protocol` re-exports this crate
//! (`pub use manox_journal as journal;`) so the retiring v2 surface keeps
//! compiling until it is deleted.

pub mod base64_bytes;
pub mod journal;

pub use journal::{
    JournalWireEntry, JournalWireEvent, ModelRef, StreamId, ThreadHeader, UsagePayload,
};
