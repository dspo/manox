// Session list shaping: pinned rows lead, newest activity first, archived
// rows move to the "more" partition.

import { describe, expect, it } from 'vitest';

import type { ThreadListItem } from '../../../protocol';
import { partitionSessions } from './sessions';

const item = (partial: Partial<ThreadListItem>): ThreadListItem => ({
  id: 't',
  title: 'Thread',
  updated_at: 0,
  running: false,
  unread: false,
  errored: false,
  pending_auth: false,
  pending_plan: false,
  background_work: false,
  model_id: 'm',
  pinned: false,
  archived: false,
  ...partial,
});

describe('partitionSessions', () => {
  it('puts pinned rows first and orders the rest by newest activity', () => {
    const { active } = partitionSessions([
      item({ id: 'old', updated_at: 100 }),
      item({ id: 'pin-old', pinned: true, updated_at: 50 }),
      item({ id: 'new', updated_at: 300 }),
      item({ id: 'pin-new', pinned: true, updated_at: 200 }),
    ]);
    expect(active.map((t) => t.id)).toEqual(['pin-new', 'pin-old', 'new', 'old']);
  });

  it('splits archived rows out of the active list', () => {
    const { active, archived } = partitionSessions([
      item({ id: 'live', updated_at: 300 }),
      item({ id: 'arch-old', archived: true, updated_at: 100 }),
      item({ id: 'arch-new', archived: true, updated_at: 200 }),
      item({ id: 'arch-pin', archived: true, pinned: true, updated_at: 50 }),
    ]);
    expect(active.map((t) => t.id)).toEqual(['live']);
    // Archived rows sort by activity only; pinning has no effect there.
    expect(archived.map((t) => t.id)).toEqual(['arch-new', 'arch-old', 'arch-pin']);
  });
});
