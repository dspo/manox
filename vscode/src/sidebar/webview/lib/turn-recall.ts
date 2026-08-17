// Pure history-recall step for the composer input, mirroring the gpui
// host's `Workspace::recall_step` (crates/agent-ui/src/workspace.rs) so the
// two hosts walk the same state machine. `turns` is newest-first; `index`
// -1 means not recalling. Being in recall requires the current value to
// still equal the recalled text, so any edit or submit exits recall
// implicitly. `none` defers to the textarea's native caret behavior.

export type RecallKey = 'up' | 'down';

export type RecallStep =
  | { kind: 'none' }
  | { kind: 'recall'; text: string }
  | { kind: 'clear' };

export function recallStep(
  key: RecallKey,
  value: string,
  index: number,
  turns: string[],
): { index: number; step: RecallStep } {
  const inRecall = index >= 0 && index < turns.length && value === turns[index];
  if (key === 'up') {
    if (inRecall) {
      if (index + 1 < turns.length) {
        return { index: index + 1, step: { kind: 'recall', text: turns[index + 1] } };
      }
      return { index, step: { kind: 'none' } };
    }
    if (value === '') {
      const newest = turns[0];
      return newest === undefined
        ? { index: -1, step: { kind: 'none' } }
        : { index: 0, step: { kind: 'recall', text: newest } };
    }
    return { index: -1, step: { kind: 'none' } };
  }
  if (inRecall) {
    if (index > 0) {
      return { index: index - 1, step: { kind: 'recall', text: turns[index - 1] } };
    }
    return { index: -1, step: { kind: 'clear' } };
  }
  return { index: -1, step: { kind: 'none' } };
}
