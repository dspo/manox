import { describe, expect, it } from 'vitest';

import { chatLayoutForWidth } from './layout';

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
});
