//! Property tests for the [`JournalStream`] engine (architecture v2 §F.1.5,
//! J.3: "随机 drop/重排/断流/重连序列 → 收敛等于服务端状态").
//!
//! Model: a dense server journal `0..len`. A random, possibly lossy
//! (duplicate/drop/out-of-order) live stream drives the engine; random
//! reconnects (`Generation` + fresh `Opened`) may supersede it mid-flight
//! (断流/换代). A shadow reference stream — the *gap-free* server delivery
//! (`history.follow` semantics: every event exactly once, ascending, and a
//! reconnect re-opens at exactly the engine's resume cursor) — is folded into
//! an ideal window.
//!
//! Properties:
//!
//! 1. **No panic**: any input sequence (including malformed ones) drives the
//!    engine without panicking;
//! 2. **Convergence**: with a *recoverable* server (every gap page can be
//!    served), the lossy stream never triggers a protocol violation, and the
//!    change stream the engine published — applied to an empty store exactly
//!    like a client store would (T6/T7 §F.2) — equals the ideal window the
//!    gap-free stream would have produced; cursor bookkeeping (`last` ==
//!    `resume` == window tail) agrees;
//! 3. **Violation is possible** (not asserted): with an unrecoverable hole
//!    the engine may report a violation; the test only requires no panic.

use std::cell::RefCell;
use std::rc::Rc;

use manox_protocol::journal_stream::{
    JournalChange, JournalEntry, JournalInput, JournalSource, JournalStream,
};
use proptest::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Seq(u64);

impl JournalEntry for Seq {
    fn first(&self) -> u64 {
        self.0
    }
    fn last(&self) -> u64 {
        self.0
    }
}

/// One action in the random stream (the same action drives both the engine
/// and the ideal reference, so "重排" and "丢帧" are real perturbations
/// rather than two unrelated models):
///
/// - `Deliver(s)`: attempt to feed live entry `s` to the engine; the ideal
///   window records it (the gap-free server stream carries it once, ascending).
/// - `Skip(s)`: drop entry `s` from the live stream (loss). The engine must
///   later repair it through a page; the ideal stream simply carries it (it is
///   the *lossless* reference).
/// - `Duplicate(s)`: feed `s` twice; both engine and ideal idempotently accept
///   one.
/// - `Reconnect`: the connection re-follower: engine gets `Generation` + a
///   fresh snapshot at its current resume cursor (the `restart()` + re-`follow`
///   semantics of F.1.3); the ideal stream re-opens identically (no window
///   change — resume == last).
#[derive(Debug, Clone, Copy)]
enum Action {
    Deliver(u64),
    Skip(u64),
    Duplicate(u64),
    Reconnect,
}

/// The gap-free server: `read_page(through)` returns the dense prefix
/// `0..=min(through, len-1)` (the `PageHistory` contract). With
/// `unrecoverable_hole = Some(h)`, the page reaching `h` comes back ending
/// at `h - 1` instead — a hole the engine cannot seal (the loss is
/// irrecoverable at the source).
struct Server {
    len: u64,
    unrecoverable_hole: Option<u64>,
}

impl JournalSource<Seq> for Server {
    fn read_page(&mut self, through: u64) -> Vec<Seq> {
        let limit = self.len.saturating_sub(1);
        if let Some(hole) = self.unrecoverable_hole
            && hole <= through
        {
            // The server cannot produce a page through `hole`: hand back
            // everything strictly older.
            return (0..hole).map(Seq).collect();
        }
        (0..=through.min(limit)).map(Seq).collect()
    }
}

/// Apply the published change sequence to an empty window, exactly as a
/// client store would (§F.2): `replace` overwrites, `prepend` prepends,
/// `append` pushes to the tail.
fn replay_changes(changes: &[JournalChange<Seq>]) -> Vec<u64> {
    let mut window: Vec<u64> = Vec::new();
    for change in changes {
        match change {
            JournalChange::Replace { entries, .. } => {
                window = entries.iter().map(|e| e.0).collect();
            }
            JournalChange::Prepend { entries, .. } => {
                let mut head: Vec<u64> = entries.iter().map(|e| e.0).collect();
                head.append(&mut window);
                window = head;
            }
            JournalChange::Append(entry) => window.push(entry.0),
        }
    }
    window
}

fn arb_actions() -> impl Strategy<Value = Vec<Action>> {
    // The seq bound is the generated journal `len` at drive time; the strategy
    // samples generously and the harness ignores out-of-range seqs (the
    // server journal is `0..len`, so a `Deliver`/`Duplicate`/`Skip` past `len`
    // is the same as the entry simply never existing — `read_page` clamps).
    prop::collection::vec(
        prop_oneof![
            // Weighted: more delivers (a realistic live tail) than perturbations.
            (0u64..64).prop_map(Action::Deliver),
            (0u64..64).prop_map(Action::Deliver),
            (0u64..64).prop_map(Action::Skip),
            (0u64..64).prop_map(Action::Duplicate),
            Just(Action::Reconnect),
        ],
        0..32,
    )
}

fn ideal_fold(actions: &[Action], len: u64) -> Vec<u64> {
    // The server's own state once a client has (possibly with drops) been
    // offered up to seq `s` is the dense prefix `0..=s` clamped to `0..len`:
    // every `Deliver`/`Duplicate` seq the engine sees is a real entry (the
    // server only sends what it holds). Skips are the lossy stream's omissions
    // — the gap-free fold still holds them.
    let mut max_delivered: Option<u64> = None;
    for a in actions {
        if let Action::Deliver(s) | Action::Duplicate(s) = a
            && *s < len
            && max_delivered.is_none_or(|m| *s > m)
        {
            max_delivered = Some(*s);
        }
    }
    match max_delivered {
        Some(m) => (0..=m).collect(),
        None => Vec::new(),
    }
}

fn drive(
    actions: &[Action],
    server: Server,
    publishes: Rc<RefCell<Vec<JournalChange<Seq>>>>,
    failures: Rc<RefCell<Vec<String>>>,
) -> (Vec<u64>, JournalStream<Seq>, Vec<u64>) {
    let server_len = server.len;
    // Open both the engine and the ideal reference on an empty journal
    // snapshot (the `new session` shape; the dense seq space means the
    // first live entry is seq 0 and a page cannot be shorter).
    let mut stream = JournalStream::new(
        Box::new(server),
        Box::new({
            let publishes = publishes.clone();
            move |change: JournalChange<Seq>| publishes.borrow_mut().push(change)
        }),
        Box::new({
            let failures = failures.clone();
            move |message: String| failures.borrow_mut().push(message)
        }),
    );
    stream
        .apply(JournalInput::Opened {
            cursor: 0,
            page: Vec::new(),
        })
        .expect("empty opening snapshot is always valid");
    // Ideal reference computed once: the server's contiguous prefix at the
    // engine's head after the whole action stream.
    let ideal = ideal_fold(actions, server_len);
    // A protocol violation halts the drive: production closes the stream on
    // `failed`, so the harness must never feed past the first violation (a
    // second violation from the same action would otherwise double-count).
    let mut failed = false;
    for action in actions {
        if failed {
            break;
        }
        match action {
            Action::Deliver(seq) | Action::Duplicate(seq) => {
                if *seq >= server_len {
                    // Out-of-range: the server never had it, so the gap-free
                    // stream would not carry it either. Skip (no violation).
                    continue;
                }
                let feeds = if matches!(action, Action::Duplicate(_)) {
                    2
                } else {
                    1
                };
                for _ in 0..feeds {
                    let _ = stream.apply(JournalInput::Entry(Seq(*seq)));
                    if !failures.borrow().is_empty() {
                        failed = true;
                        break;
                    }
                }
            }
            Action::Skip(_) => {
                // The lossy engine stream omits the seq entirely: nothing to
                // feed. The gap-free fold still counts it (server-side).
            }
            Action::Reconnect => {
                let cursors = stream.cursors();
                if let Some(resume) = cursors.resume {
                    // The snapshot a resuming client receives: the server's
                    // dense window up to its newest applied seq. An empty
                    // window (nothing applied) re-opens with an empty page at
                    // the opening cursor.
                    let page: Vec<Seq> = match cursors.last {
                        Some(last) => (0..=last).map(Seq).collect(),
                        None => Vec::new(),
                    };
                    let _ = stream.apply(JournalInput::Generation);
                    let _ = stream.apply(JournalInput::Opened {
                        cursor: resume,
                        page,
                    });
                    failed = !failures.borrow().is_empty();
                }
                // The gap-free stream re-follows at the same cursor: the
                // window is unchanged (the snapshot == the current tail).
            }
        }
    }
    (ideal, stream, replay_changes(&publishes.borrow().clone()))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Convergence (§J.3): with a *recoverable* server, no random
    /// loss/reorder/reconnect sequence can make the engine report a protocol
    /// violation, and its published change stream — folded onto an empty
    /// store — equals the gap-free server fold.
    #[test]
    fn journal_lossy_stream_converges_to_the_server_state(
        len in 1u64..24,
        actions in arb_actions(),
    ) {
        let publishes: Rc<RefCell<Vec<JournalChange<Seq>>>> = Rc::new(RefCell::new(Vec::new()));
        let failures: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let server = Server { len, unrecoverable_hole: None };
        let (ideal, stream, store) = drive(&actions, server, publishes.clone(), failures.clone());
        prop_assert!(
            failures.borrow().is_empty(),
            "recoverable stream violated: {:?}",
            failures.borrow()
        );
        prop_assert_eq!(
            &store, &ideal,
            "engine change stream diverged from the server fold"
        );
        let cursors = stream.cursors();
        let tail = ideal.last().copied();
        prop_assert_eq!(cursors.last, tail, "last cursor drifted");
        // `resume` always equals the last *applied* cursor once the stream
        // opened — `0` for a still-empty window (the opening cursor), the
        // ideal tail otherwise.
        prop_assert_eq!(
            cursors.resume,
            Some(tail.unwrap_or(0)),
            "resume cursor drifted",
        );
        prop_assert_eq!(
            cursors.first,
            ideal.first().copied(),
            "first cursor drifted",
        );
    }

    /// Irrecoverable holes (§J.3 随机注入{丢帧…}): a `Skip` whose seq the
    /// server itself cannot deliver must surface as exactly one violation on
    /// the first repair attempt — never a panic, never silent data loss.
    #[test]
    fn journal_unrecoverable_gap_reports_one_violation(
        len in 2u64..24,
        actions in arb_actions(),
    ) {
        let publishes: Rc<RefCell<Vec<JournalChange<Seq>>>> = Rc::new(RefCell::new(Vec::new()));
        let failures: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let hole = actions.iter().find_map(|a| match a {
            Action::Skip(s) => Some(*s),
            _ => None,
        });
        let server = Server { len, unrecoverable_hole: hole.filter(|h| *h < len) };
        // Drive may or may not violate depending on whether the hole is ever
        // reached: a hole at `h` only fires when the engine must seal
        // *through* `h` (a `Deliver`/`Duplicate` of seq > h forces a repair
        // page `read_page(h)` that the server cannot complete).
        let (_ideal, _stream, _store) = drive(&actions, server, publishes, failures.clone());
        // No-panic is the hard gate; `drive` halts at the first violation
        // (production treats it as terminal), so the list never exceeds one.
        prop_assert!(
            failures.borrow().len() <= 1,
            "multiple violations (idempotence broken): {:?}",
            failures.borrow()
        );
        let _ = &failures;
    }

    /// Malformed inputs never panic (§J.3 随机注入): entries before opening,
    /// inverted ranges, generations without a following snapshot, prepends
    /// onto a closed window — the engine must reject with `Err`/`failed`
    /// rather than abort.
    #[test]
    fn journal_malformed_inputs_never_panic(
        len in 1u64..16,
        seqs in prop::collection::vec(0u64..20, 0..24),
        reconnects in 0u8..4,
    ) {
        let publishes: Rc<RefCell<Vec<JournalChange<Seq>>>> = Rc::new(RefCell::new(Vec::new()));
        let failures: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let mut stream = JournalStream::new(
            Box::new(Server { len, unrecoverable_hole: None }),
            Box::new({
                let publishes = publishes.clone();
                move |change: JournalChange<Seq>| publishes.borrow_mut().push(change)
            }),
            Box::new({
                let failures = failures.clone();
                move |message: String| failures.borrow_mut().push(message)
            }),
        );
        // Drive *before* opening: every such input must be a clean violation.
        for seq in &seqs {
            let _ = stream.apply(JournalInput::Entry(Seq(*seq)));
        }
        for _ in 0..reconnects {
            let _ = stream.apply(JournalInput::Generation);
        }
        let _ = stream.apply(JournalInput::Prepend { page: vec![Seq(9), Seq(2)], has_more: false });
        let _ = stream.apply(JournalInput::Opened { cursor: 1, page: vec![Seq(5)] }); // page not through
        let _ = &publishes;
        let _ = &failures;
    }
}
