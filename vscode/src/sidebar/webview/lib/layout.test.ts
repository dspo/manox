import { describe, expect, it } from 'vitest';

import {
  chatLayoutForWidth,
  CONVERSATION_MIN_PX,
  INFO_PANEL_WIDTH_PX,
  maxSessionListWidth,
  NON_FLEX_OVERHEAD_PX,
  THREE_COL_BREAKPOINT_PX,
} from './layout';

describe('chatLayoutForWidth', () => {
  it('conversation only below the two-column breakpoint', () => {
    expect(chatLayoutForWidth(0)).toBe('conversation');
    expect(chatLayoutForWidth(759)).toBe('conversation');
  });
  it('adds the info column at the two-column breakpoint', () => {
    expect(chatLayoutForWidth(760)).toBe('conversation-info');
    expect(chatLayoutForWidth(1119)).toBe('conversation-info');
  });
  it('adds the session list at the three-column breakpoint', () => {
    expect(chatLayoutForWidth(1120)).toBe('list-conversation-info');
    expect(chatLayoutForWidth(2000)).toBe('list-conversation-info');
  });
  it('never lets the session list squeeze the conversation below its minimum', () => {
    for (const width of [THREE_COL_BREAKPOINT_PX, 1600, 2000]) {
      const conversation =
        width - maxSessionListWidth(width) - INFO_PANEL_WIDTH_PX - NON_FLEX_OVERHEAD_PX;
      expect(conversation).toBeGreaterThanOrEqual(CONVERSATION_MIN_PX);
    }
  });
});
