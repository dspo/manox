// Thread-row status machine: priority order and flag mapping, mirroring the
// gpui sidebar's ship-wheel state machine.

import { describe, expect, it } from 'vitest';

import type { ThreadListItem } from '../../../protocol';
import { threadRowState } from './thread-status';

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

describe('threadRowState', () => {
  it('flags an errored thread even while running', () => {
    expect(threadRowState(item({ errored: true, running: true }))).toBe('errored');
  });

  it('maps every waiting-user signal to the blue static wheel', () => {
    expect(threadRowState(item({ pending_auth: true }))).toBe('waiting');
    expect(threadRowState(item({ pending_plan: true }))).toBe('waiting');
    expect(threadRowState(item({ pending_auth: true, pending_plan: true }))).toBe('waiting');
  });

  it('maps self-advancing states to the spinning wheel', () => {
    expect(threadRowState(item({ running: true }))).toBe('autonomous');
    expect(threadRowState(item({ background_work: true }))).toBe('autonomous');
    expect(threadRowState(item({ running: true, background_work: true }))).toBe('autonomous');
  });

  it('keeps a pending authorization blue-static even while the turn runs', () => {
    expect(threadRowState(item({ pending_auth: true, running: true }))).toBe('waiting');
  });

  it('maps an unread thread to the blue static wheel', () => {
    expect(threadRowState(item({ unread: true }))).toBe('unread');
  });

  it('falls back to the idle gray wheel', () => {
    expect(threadRowState(item({}))).toBe('idle');
  });
});
