import { describe, expect, it } from 'vitest';

import {
  chatLayoutForWidth,
  CONVERSATION_MIN_PX,
  INFO_CARD_GUTTER_PX,
  maxSessionListWidth,
  SASH_PX,
  SESSION_LIST_BREAKPOINT_PX,
} from './layout';

describe('chatLayoutForWidth', () => {
  it('conversation only below the info-card breakpoint', () => {
    expect(chatLayoutForWidth(0)).toBe('conversation');
    expect(chatLayoutForWidth(775)).toBe('conversation');
  });
  it('adds the info card at its breakpoint', () => {
    expect(chatLayoutForWidth(776)).toBe('conversation-info');
    expect(chatLayoutForWidth(1119)).toBe('conversation-info');
  });
  it('adds the session list at its breakpoint', () => {
    expect(chatLayoutForWidth(1120)).toBe('list-conversation-info');
    expect(chatLayoutForWidth(2000)).toBe('list-conversation-info');
  });
  it('never lets the session list squeeze the message content below its minimum', () => {
    for (const width of [SESSION_LIST_BREAKPOINT_PX, 1600, 2000]) {
      const content = width - maxSessionListWidth(width) - SASH_PX - INFO_CARD_GUTTER_PX;
      expect(content).toBeGreaterThanOrEqual(CONVERSATION_MIN_PX);
    }
    // The list cap reserves the card gutter; at the breakpoint it tops out
    // at 1120 - 480 - 296 - 4 = 340, so the conversation column holds both.
    expect(maxSessionListWidth(SESSION_LIST_BREAKPOINT_PX)).toBe(340);
  });
});
