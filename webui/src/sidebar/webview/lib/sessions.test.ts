// Session list shaping: pinned rows lead, newest activity first, archived
// rows move to the "more" partition, and team members nest under their
// leader.

import { describe, expect, it } from 'vitest';

import type { ThreadListItem } from '../../../protocol';
import { partitionSessions, type SessionTreeNode } from './sessions';

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
  parent_id: null,
  depth: 0,
  ...partial,
});

const ids = (nodes: SessionTreeNode[]): string[] => nodes.map((n) => n.item.id);

describe('partitionSessions', () => {
  it('puts pinned rows first and orders the rest by newest activity', () => {
    const { active } = partitionSessions([
      item({ id: 'old', updated_at: 100 }),
      item({ id: 'pin-old', pinned: true, updated_at: 50 }),
      item({ id: 'new', updated_at: 300 }),
      item({ id: 'pin-new', pinned: true, updated_at: 200 }),
    ]);
    expect(ids(active)).toEqual(['pin-new', 'pin-old', 'new', 'old']);
  });

  it('splits archived rows out of the active list', () => {
    const { active, archived } = partitionSessions([
      item({ id: 'live', updated_at: 300 }),
      item({ id: 'arch-old', archived: true, updated_at: 100 }),
      item({ id: 'arch-new', archived: true, updated_at: 200 }),
      item({ id: 'arch-pin', archived: true, pinned: true, updated_at: 50 }),
    ]);
    expect(ids(active)).toEqual(['live']);
    // Archived rows sort by activity only; pinning has no effect there.
    expect(ids(archived)).toEqual(['arch-new', 'arch-old', 'arch-pin']);
  });

  it('nests team members under their leader in activity order', () => {
    const { active } = partitionSessions([
      item({ id: 'leader', updated_at: 100 }),
      item({ id: 'member-old', parent_id: 'leader', depth: 1, updated_at: 50 }),
      item({ id: 'member-new', parent_id: 'leader', depth: 1, updated_at: 200 }),
    ]);
    expect(ids(active)).toEqual(['leader']);
    const leader = active[0];
    expect(ids(leader.children)).toEqual(['member-new', 'member-old']);
  });

  it('keeps an orphan (leader missing from the list) top-level', () => {
    const { active } = partitionSessions([
      item({ id: 'member', parent_id: 'gone', depth: 1 }),
    ]);
    expect(ids(active)).toEqual(['member']);
    expect(active[0].children).toEqual([]);
  });

  it('shapes archived rows as their own forest', () => {
    const { active, archived } = partitionSessions([
      item({ id: 'arch-leader', archived: true, updated_at: 100 }),
      item({ id: 'arch-member', archived: true, parent_id: 'arch-leader', depth: 1 }),
    ]);
    expect(ids(active)).toEqual([]);
    expect(ids(archived)).toEqual(['arch-leader']);
    expect(ids(archived[0].children)).toEqual(['arch-member']);
  });

  it('nests only at the stated depth without infinite recursion on cycles', () => {
    // The store zeroes `depth` on a cycle, so corrupt wire data (a points at
    // b, b points at a) must not hang the forest builder: both rows stay
    // top-level and neither gains children.
    const { active } = partitionSessions([
      item({ id: 'a', parent_id: 'b', depth: 0 }),
      item({ id: 'b', parent_id: 'a', depth: 0 }),
    ]);
    expect(ids(active)).toEqual(['a', 'b']);
    expect(active[0].children).toEqual([]);
    expect(active[1].children).toEqual([]);
  });
});
