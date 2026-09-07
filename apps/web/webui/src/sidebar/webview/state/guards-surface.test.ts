// C3/J.5: the guards' declaration-surface tables must equal the
// Rust-generated fixture — the single source is
// crates/manox-protocol/src/surface.rs (macro-generated from the wire
// enums, compiler-enforced exhaustive). The former hand-copied mirror
// arrays were the exact drift class the unknown-tag tolerance makes
// invisible at runtime: a tag present in Rust but missing here is
// silently dropped by the parsers, and no test noticed.

import { describe, expect, it } from 'vitest';

import surfaceTags from '../../../../../../../crates/manox-protocol/fixtures/surface-tags.json';
import {
  HOST_EVENT_TAGS,
  JOURNAL_ENTRY_TAGS,
} from '../../../../../../../crates/manox-protocol/bindings/guards';

describe('guards declaration surfaces sync with the Rust source', () => {
  it('journal entry tags equal JOURNAL_ENTRIES', () => {
    expect([...JOURNAL_ENTRY_TAGS]).toEqual(surfaceTags.journalEntries);
  });

  it('host event tags equal HOST_EVENTS', () => {
    expect([...HOST_EVENT_TAGS]).toEqual(surfaceTags.hostEvents);
  });
});
