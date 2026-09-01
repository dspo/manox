import { describe, expect, it } from 'vitest';

import { enterAction, type EnterKeyInfo } from './ime';

const enter = (over: Partial<EnterKeyInfo> = {}): EnterKeyInfo => ({
  key: 'Enter',
  shiftKey: false,
  isComposing: false,
  keyCode: 13,
  ...over,
});

describe('enterAction', () => {
  it('submits on a plain Enter', () => {
    expect(enterAction(enter(), false)).toBe('submit');
  });

  it('defers while an IME composition is in progress', () => {
    expect(enterAction(enter({ isComposing: true }), false)).toBe('defer');
  });

  it('defers WebKit composition keycodes', () => {
    expect(enterAction(enter({ keyCode: 229 }), false)).toBe('defer');
  });

  it('defers shift+Enter (newline)', () => {
    expect(enterAction(enter({ shiftKey: true }), false)).toBe('defer');
  });

  it('defers the trailing Enter after a composition ended', () => {
    expect(enterAction(enter(), true)).toBe('defer');
  });

  it('defers non-Enter keys', () => {
    expect(enterAction(enter({ key: 'a' }), false)).toBe('defer');
  });
});
