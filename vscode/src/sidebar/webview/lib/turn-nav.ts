// Searchable navigation over the thread's user turns. Pure functions — the
// overlay component feeds them the transcript items and owns all keyboard
// state, so this module stays trivially testable in node.

import type { TranscriptItem } from '../state/transcript';
import { t } from './i18n';

export interface TurnEntry {
  id: string;
  text: string;
  display: string;
}

/** Collapse every run of whitespace to a single space (mirrors the gpui
 * host's `split_whitespace().join(" ")`). */
const collapseWhitespace = (text: string): string =>
  text
    .split(/\s+/)
    .filter(Boolean)
    .join(' ');

/** User turns in reverse transcript order (newest first). `display` is the
 * single-line text shown in the list; attachment-only and empty turns fall
 * back to localized placeholders. */
export function collectUserTurns(items: TranscriptItem[]): TurnEntry[] {
  return items
    .filter((item): item is Extract<TranscriptItem, { kind: 'user' }> => item.kind === 'user')
    .reverse()
    .map((item) => {
      const collapsed = collapseWhitespace(item.displayText || item.text);
      const display =
        collapsed !== ''
          ? collapsed
          : item.images && item.images.length > 0
            ? t('turn_navigator_attachment_only')
            : t('turn_navigator_empty_message');
      return { id: item.id, text: item.text, display };
    });
}

/** Indices into `turns` matching the query. An empty query keeps every turn;
 * otherwise the raw text is matched case-insensitively (mirrors the gpui
 * host's `filter_turns`), preserving the newest-first order. */
export function filterTurns(turns: TurnEntry[], query: string): number[] {
  const q = query.toLowerCase();
  if (q === '') return turns.map((_, i) => i);
  return turns
    .map((turn, i) => ({ i, hit: turn.text.toLowerCase().includes(q) }))
    .filter(({ hit }) => hit)
    .map(({ i }) => i);
}
