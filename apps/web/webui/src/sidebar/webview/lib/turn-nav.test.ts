import { afterEach, describe, expect, it } from 'vitest';

import { setLanguageForTest, t } from './i18n';
import { collectUserTurns, filterTurns, type TurnEntry } from './turn-nav';
import type { TranscriptItem } from '../state/transcript';

const user = (
  id: string,
  text: string,
  extra: Partial<Extract<TranscriptItem, { kind: 'user' }>> = {},
): TranscriptItem => ({ kind: 'user', id, text, ...extra });

const assistant = (id: string): TranscriptItem => ({ kind: 'assistant', id, text: 'reply' });

const ATTACHMENT = [{ mimeType: 'image/png', data: null, byteLen: null }];

describe('collectUserTurns', () => {
  afterEach(() => setLanguageForTest('en'));

  it('collects user turns newest first, skipping non-user items', () => {
    const items = [user('old', 'old question'), assistant('a1'), user('new', 'new question')];
    const turns = collectUserTurns(items);
    expect(turns.map((turn) => turn.id)).toEqual(['new', 'old']);
    expect(turns.map((turn) => turn.text)).toEqual(['new question', 'old question']);
  });

  it('collapses multiline whitespace for single-line display', () => {
    const turns = collectUserTurns([user('u1', '  first\n\n second\tthird  ')]);
    expect(turns[0].display).toBe('first second third');
  });

  it('prefers displayText over raw text (slash invocations)', () => {
    const turns = collectUserTurns([user('u1', '/exit', { displayText: '/exit archived\n now' })]);
    expect(turns[0].display).toBe('/exit archived now');
  });

  it('falls back to a localized placeholder for attachment-only turns', () => {
    expect(collectUserTurns([user('u1', '', { images: ATTACHMENT })])[0].display).toBe(
      'Attachment-only message',
    );
    setLanguageForTest('zh-cn');
    expect(collectUserTurns([user('u1', '', { images: ATTACHMENT })])[0].display).toBe('仅附件消息');
  });

  it('falls back to a localized placeholder for empty turns', () => {
    expect(collectUserTurns([user('u1', '   ')])[0].display).toBe('Empty message');
    setLanguageForTest('zh-cn');
    expect(collectUserTurns([user('u1', '   ')])[0].display).toBe('空消息');
  });
});

describe('filterTurns', () => {
  const turns: TurnEntry[] = [
    { id: 'new', text: 'Latest line\nHidden Needle', display: 'Latest line Hidden Needle' },
    { id: 'mid', text: 'older NEEDLE', display: 'older NEEDLE' },
    { id: 'old', text: 'unrelated', display: 'unrelated' },
  ];

  it('keeps every index for an empty query', () => {
    expect(filterTurns(turns, '')).toEqual([0, 1, 2]);
  });

  it('does not treat whitespace-only queries as empty (gpui parity)', () => {
    expect(filterTurns(turns, '   ')).toEqual([]);
  });

  it('matches substrings case-insensitively without reordering', () => {
    expect(filterTurns(turns, 'needle')).toEqual([0, 1]);
    expect(filterTurns(turns, 'NEEDLE')).toEqual([0, 1]);
    expect(filterTurns(turns, 'hidden')).toEqual([0]);
  });

  it('returns no indices when nothing matches', () => {
    expect(filterTurns(turns, 'zzz')).toEqual([]);
    expect(filterTurns([], 'zzz')).toEqual([]);
  });
});

// Type-level sanity: the i18n keys used by the navigator exist in the dict.
describe('navigator i18n keys', () => {
  it('resolves every navigator key in both locales', () => {
    setLanguageForTest('en');
    expect(t('turn_navigator_title')).toBe('Search user messages');
    expect(t('turn_navigator_search_placeholder')).toBe('Search user messages…');
    expect(t('turn_navigator_empty')).toBe('No user messages');
    expect(t('turn_navigator_no_results')).toBe('No matching messages');
    setLanguageForTest('zh-cn');
    expect(t('turn_navigator_title')).toBe('搜索用户消息');
    expect(t('turn_navigator_search_placeholder')).toBe('搜索用户消息…');
    expect(t('turn_navigator_empty')).toBe('暂无用户消息');
    expect(t('turn_navigator_no_results')).toBe('没有匹配的消息');
  });
});
