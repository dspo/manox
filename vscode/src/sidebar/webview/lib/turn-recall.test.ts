import { describe, expect, it } from 'vitest';

import { recallStep } from './turn-recall';

const up = (value: string, index: number, turns: string[]) =>
  recallStep('up', value, index, turns);
const down = (value: string, index: number, turns: string[]) =>
  recallStep('down', value, index, turns);

describe('recallStep up', () => {
  it('starts from the newest turn on an empty input', () => {
    const out = up('', -1, ['newest', 'oldest']);
    expect(out).toEqual({ index: 0, step: { kind: 'recall', text: 'newest' } });
  });

  it('walks further back while recalling', () => {
    expect(up('newest', 0, ['newest', 'middle', 'oldest'])).toEqual({
      index: 1,
      step: { kind: 'recall', text: 'middle' },
    });
    expect(up('middle', 1, ['newest', 'middle', 'oldest'])).toEqual({
      index: 2,
      step: { kind: 'recall', text: 'oldest' },
    });
  });

  it('clamps at the oldest turn', () => {
    expect(up('oldest', 2, ['newest', 'middle', 'oldest'])).toEqual({
      index: 2,
      step: { kind: 'none' },
    });
  });

  it('defers with typed text outside recall', () => {
    expect(up('typed draft', -1, ['newest'])).toEqual({ index: -1, step: { kind: 'none' } });
  });

  it('defers with an empty history', () => {
    expect(up('', -1, [])).toEqual({ index: -1, step: { kind: 'none' } });
  });

  it('treats an edited recall value as fresh', () => {
    expect(up('newest edited', 0, ['newest', 'oldest'])).toEqual({
      index: -1,
      step: { kind: 'none' },
    });
  });

  it('treats a stale index from another thread as a fresh recall', () => {
    expect(up('', 5, ['newest', 'oldest'])).toEqual({
      index: 0,
      step: { kind: 'recall', text: 'newest' },
    });
  });
});

describe('recallStep down', () => {
  it('walks toward newer turns while recalling', () => {
    expect(down('oldest', 2, ['newest', 'middle', 'oldest'])).toEqual({
      index: 1,
      step: { kind: 'recall', text: 'middle' },
    });
    expect(down('middle', 1, ['newest', 'middle', 'oldest'])).toEqual({
      index: 0,
      step: { kind: 'recall', text: 'newest' },
    });
  });

  it('clears the input at the newest turn', () => {
    expect(down('newest', 0, ['newest', 'oldest'])).toEqual({ index: -1, step: { kind: 'clear' } });
  });

  it('defers outside recall', () => {
    expect(down('', -1, ['newest'])).toEqual({ index: -1, step: { kind: 'none' } });
  });
});
