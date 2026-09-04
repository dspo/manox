//! Shared-vector conformance tests for the [`JournalStream`] engine (T3).
//!
//! Loads `crates/manox-protocol/test-vectors/journal-cases.json` — the same
//! file the webui vitest suite loads (`apps/web/webui/src/sidebar/webview/
//! state/journal.test.ts`) — and drives the engine case by case, asserting the
//! exact publish sequence and the first protocol violation. The TS twin engine
//! in `state/journal.ts` must stay behaviourally identical; the vectors are the
//! equivalence contract.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use manox_protocol::journal::{
    JournalChange, JournalEntry, JournalInput, JournalSource, JournalStream,
};
use serde::Deserialize;

/// A minimal journal entry: an inclusive `[start, end]` seq span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    start: u64,
    end: u64,
}

impl Entry {
    fn at(seq: u64) -> Self {
        Self {
            start: seq,
            end: seq,
        }
    }
}

impl JournalEntry for Entry {
    fn first(&self) -> u64 {
        self.start
    }
    fn last(&self) -> u64 {
        self.end
    }
}

/// The vector's entry item: a bare seq or a `{first,last}` span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EntrySpec {
    first: u64,
    last: u64,
}

impl EntrySpec {
    fn to_entry(self) -> Entry {
        Entry {
            start: self.first,
            end: self.last,
        }
    }
}

impl<'de> Deserialize<'de> for EntrySpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Num(u64),
            Obj { first: u64, last: u64 },
        }
        match Raw::deserialize(deserializer)? {
            Raw::Num(seq) => Ok(Self {
                first: seq,
                last: seq,
            }),
            Raw::Obj { first, last } => Ok(Self { first, last }),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
enum Event {
    Opened {
        cursor: u64,
        #[serde(default)]
        entries: Vec<EntrySpec>,
    },
    Entry {
        #[serde(default)]
        seq: Option<u64>,
        #[serde(default)]
        first: Option<u64>,
        #[serde(default)]
        last: Option<u64>,
    },
    Prepend {
        #[serde(default)]
        entries: Vec<EntrySpec>,
        #[serde(default)]
        has_more: bool,
    },
    Generation,
    /// Declares a gap-repair page the fake source serves when the engine
    /// requests `read_page(through)` (consumed in declaration order).
    GapRepair {
        through: u64,
        #[serde(default)]
        entries: Vec<EntrySpec>,
    },
    /// Declares the full journal the fake source serves any `read_page` from
    /// (last resort after `gap-repair` overrides).
    Journal {
        #[serde(default)]
        entries: Vec<EntrySpec>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum PublishSpec {
    Replace {
        #[serde(default)]
        entries: Vec<EntrySpec>,
        #[serde(default)]
        has_more: bool,
    },
    Prepend {
        #[serde(default)]
        entries: Vec<EntrySpec>,
        #[serde(default)]
        has_more: bool,
    },
    Append {
        #[serde(default)]
        seq: Option<u64>,
        #[serde(default)]
        first: Option<u64>,
        #[serde(default)]
        last: Option<u64>,
    },
}

impl PublishSpec {
    fn to_change(&self) -> JournalChange<Entry> {
        match self {
            Self::Replace { entries, has_more } => JournalChange::Replace {
                entries: entries.iter().map(|s| s.to_entry()).collect(),
                has_more: *has_more,
            },
            Self::Prepend { entries, has_more } => JournalChange::Prepend {
                entries: entries.iter().map(|s| s.to_entry()).collect(),
                has_more: *has_more,
            },
            Self::Append { seq, first, last } => {
                let (start, end) = entry_bounds(*seq, *first, *last);
                JournalChange::Append(Entry { start, end })
            }
        }
    }
}

fn entry_bounds(seq: Option<u64>, first: Option<u64>, last: Option<u64>) -> (u64, u64) {
    match (seq, first, last) {
        (Some(s), None, None) => (s, s),
        (None, Some(f), Some(l)) => (f, l),
        _ => unreachable!("vector entry must be `seq` or `first`+`last`"),
    }
}

/// Optional cursor bookkeeping to assert after all events are applied
/// (rule §F.1.4: first/last/resume recorded on every successful apply).
#[derive(Debug, Deserialize)]
struct CursorsSpec {
    first: Option<u64>,
    last: Option<u64>,
    resume: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    name: String,
    events: Vec<Event>,
    #[serde(default)]
    expected_publish: Vec<PublishSpec>,
    #[serde(default)]
    expected_fail: Option<String>,
    #[serde(default)]
    expected_cursors: Option<CursorsSpec>,
}

/// Fake `PageHistory`: serves explicit `gap-repair` pages (per `through`,
/// queue per duplicate through), falling back to a sparse journal.
struct VectorSource {
    overrides: BTreeMap<u64, Vec<Vec<Entry>>>,
    journal: BTreeMap<u64, Entry>,
    name: &'static str,
}

impl JournalSource<Entry> for VectorSource {
    fn read_page(&mut self, through: u64) -> Vec<Entry> {
        if let Some(page) = self
            .overrides
            .get_mut(&through)
            .and_then(|queue| (!queue.is_empty()).then(|| queue.remove(0)))
        {
            return page;
        }
        (0..=through)
            .filter_map(|s| self.journal.get(&s).copied())
            .collect()
    }

    fn name(&self) -> &str {
        self.name
    }
}

struct Harness {
    publishes: Rc<RefCell<Vec<JournalChange<Entry>>>>,
    failures: Rc<RefCell<Vec<String>>>,
}

impl Harness {
    fn new(
        publishes: Rc<RefCell<Vec<JournalChange<Entry>>>>,
        failures: Rc<RefCell<Vec<String>>>,
    ) -> Self {
        Self {
            publishes,
            failures,
        }
    }

    fn stream_with(&self, source: VectorSource) -> JournalStream<Entry> {
        JournalStream::new(
            Box::new(source),
            Box::new({
                let publishes = self.publishes.clone();
                move |change: JournalChange<Entry>| publishes.borrow_mut().push(change)
            }),
            Box::new({
                let failures = self.failures.clone();
                move |message: String| failures.borrow_mut().push(message)
            }),
        )
    }
}

fn load_vectors() -> Vec<Case> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/test-vectors/journal-cases.json"
    );
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("cannot read {path}: {error}"));
    serde_json::from_str(&raw).unwrap_or_else(|error| panic!("invalid vector file {path}: {error}"))
}

#[test]
fn journal_test_vectors_conformance() {
    let vectors = load_vectors();
    assert!(!vectors.is_empty(), "the vector file must not be empty");
    for case in &vectors {
        let publishes = Rc::new(RefCell::new(Vec::<JournalChange<Entry>>::new()));
        let failures = Rc::new(RefCell::new(Vec::<String>::new()));
        let harness = Harness::new(publishes.clone(), failures.clone());

        let mut overrides: BTreeMap<u64, Vec<Vec<Entry>>> = BTreeMap::new();
        let mut journal: BTreeMap<u64, Entry> = BTreeMap::new();
        for event in &case.events {
            match event {
                Event::GapRepair { through, entries } => {
                    overrides
                        .entry(*through)
                        .or_default()
                        .push(entries.iter().map(|s| s.to_entry()).collect());
                }
                Event::Journal { entries } => {
                    for spec in entries {
                        journal.insert(spec.first, spec.to_entry());
                    }
                }
                _ => {}
            }
        }
        let mut stream = harness.stream_with(VectorSource {
            overrides,
            journal,
            name: "journal",
        });

        for event in &case.events {
            let input = match event {
                Event::Opened { cursor, entries } => JournalInput::Opened {
                    cursor: *cursor,
                    page: entries.iter().map(|s| s.to_entry()).collect(),
                },
                Event::Entry { seq, first, last } => {
                    let (start, end) = entry_bounds(*seq, *first, *last);
                    JournalInput::Entry(Entry { start, end })
                }
                Event::Prepend { entries, has_more } => JournalInput::Prepend {
                    page: entries.iter().map(|s| s.to_entry()).collect(),
                    has_more: *has_more,
                },
                Event::Generation => JournalInput::Generation,
                Event::GapRepair { .. } | Event::Journal { .. } => continue,
            };
            let result = stream.apply(input);
            if let Some(expected) = &case.expected_fail
                && result.is_err()
            {
                assert!(
                    failures.borrow().iter().any(|f| f == expected),
                    "case `{}`: first violation must match expected_fail `{expected}`",
                    case.name
                );
                break;
            }
            result.unwrap_or_else(|error| {
                panic!(
                    "case `{}`: unexpected protocol violation: {error}",
                    case.name
                )
            });
        }

        assert_eq!(
            *publishes.borrow(),
            case.expected_publish
                .iter()
                .map(PublishSpec::to_change)
                .collect::<Vec<_>>(),
            "case `{}`: publish sequence mismatch",
            case.name
        );
        if let Some(expected) = &case.expected_fail {
            assert_eq!(
                failures.borrow().first().map(String::as_str),
                Some(expected.as_str()),
                "case `{}`: expected_fail mismatch",
                case.name
            );
        } else {
            assert!(
                failures.borrow().is_empty(),
                "case `{}`: unexpected failures: {:?}",
                case.name,
                *failures.borrow()
            );
        }
        if let Some(expected) = &case.expected_cursors {
            let got = stream.cursors();
            assert_eq!(
                (got.first, got.last, got.resume),
                (expected.first, expected.last, expected.resume),
                "case `{}`: cursor bookkeeping mismatch",
                case.name
            );
        }
    }
}

/// A tiny hand-driven sanity pass independent of the shared vectors: the
/// golden path open → contiguous append → stale drop → gap repair → prepend.
#[test]
fn journal_engine_golden_path() {
    let publishes = Rc::new(RefCell::new(Vec::<JournalChange<Entry>>::new()));
    let failures = Rc::new(RefCell::new(Vec::<String>::new()));
    let harness = Harness::new(publishes.clone(), failures.clone());
    let source = VectorSource {
        overrides: BTreeMap::new(),
        journal: (0..=7).map(|s| (s, Entry::at(s))).collect(),
        name: "journal",
    };
    let mut stream = harness.stream_with(source);

    stream
        .apply(JournalInput::Opened {
            cursor: 3,
            page: (0..=3).map(Entry::at).collect(),
        })
        .unwrap();
    stream.apply(JournalInput::Entry(Entry::at(2))).unwrap(); // stale: silently dropped
    stream.apply(JournalInput::Entry(Entry::at(7))).unwrap(); // gap: repaired through the full journal 4..=6 + 7
    stream
        .apply(JournalInput::Prepend {
            page: Vec::new(),
            has_more: false,
        })
        .unwrap(); // empty page is always accepted (end-of-history no-op)

    let published = publishes.borrow();
    assert!(matches!(
        &published[0],
        JournalChange::Replace { entries, has_more: false } if entries.len() == 4
    ));
    assert!(matches!(
        &published[1],
        JournalChange::Replace { entries, has_more: false } if entries.iter().map(JournalEntry::first).eq(0..=7)
    ));
    assert!(matches!(
        published[2],
        JournalChange::Prepend { ref entries, has_more: false } if entries.is_empty()
    ));
    assert!(failures.borrow().is_empty());
    let cursors = stream.cursors();
    assert_eq!(cursors.resume, Some(7));
    assert_eq!(cursors.last, Some(7));
}
