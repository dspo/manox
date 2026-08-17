// Centered, searchable navigation over the thread's user turns. Owns the
// query/filter/selection state and the keyboard loop — up/down wrap around
// the filtered rows, enter navigates, cmd/ctrl-c copies the selected turn,
// escape closes. The parent renders it inside the transcript container and
// owns the backdrop and the scroll jump.

import { Search } from 'lucide-react';
import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';

import { t } from '../lib/i18n';
import { filterTurns, type TurnEntry } from '../lib/turn-nav';
import { cn } from '../lib/utils';

export type TurnNavigatorProps = {
  /** Newest-first user turns (see `collectUserTurns`). */
  turns: TurnEntry[];
  onNavigate: (id: string) => void;
  onClose: () => void;
};

/** Copy plain text into the clipboard; webviews may lack the async API, so
 * fall back to the legacy execCommand path on a hidden textarea. */
function copyText(text: string): boolean {
  if (navigator.clipboard) {
    void navigator.clipboard.writeText(text);
    return true;
  }
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.position = 'fixed';
  ta.style.opacity = '0';
  document.body.appendChild(ta);
  ta.select();
  const ok = document.execCommand('copy');
  ta.remove();
  return ok;
}

export const TurnNavigator = ({ turns, onNavigate, onClose }: TurnNavigatorProps) => {
  const [query, setQuery] = useState('');
  const [selected, setSelected] = useState(0);
  const [copied, setCopied] = useState(false);
  const filtered = useMemo(() => filterTurns(turns, query), [turns, query]);
  const rowRefs = useRef<(HTMLButtonElement | null)[]>([]);

  // Mirrors the gpui host's `ScrollStrategy::Nearest`: keep the highlighted
  // row visible as selection moves under the keyboard.
  useEffect(() => {
    rowRefs.current[selected]?.scrollIntoView({ block: 'nearest' });
  }, [selected, filtered]);

  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 1500);
    return () => clearTimeout(timer);
  }, [copied]);

  const handleKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      if (filtered.length === 0) return;
      const delta = e.key === 'ArrowDown' ? 1 : -1;
      setSelected((prev) => (prev + delta + filtered.length) % filtered.length);
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const entryIx = filtered[selected];
      if (entryIx !== undefined) onNavigate(turns[entryIx].id);
    } else if (e.key === 'Escape') {
      onClose();
    } else if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'c') {
      const entryIx = filtered[selected];
      if (entryIx === undefined) return;
      e.preventDefault();
      if (copyText(turns[entryIx].text)) setCopied(true);
    }
  };

  return (
    <div
      className="bg-card flex max-h-[70%] w-[min(26rem,90%)] flex-col overflow-hidden rounded-md border shadow-md"
      onClick={(e) => e.stopPropagation()}
      role="dialog"
    >
      <div className="flex items-center gap-2 border-b border-border px-2">
        <Search className="text-muted-foreground size-4 shrink-0" />
        <input
          autoFocus
          className="w-full bg-transparent py-2 text-sm outline-none"
          onChange={(e) => {
            setQuery(e.target.value);
            setSelected(0);
          }}
          onKeyDown={handleKeyDown}
          placeholder={t('turn_navigator_search_placeholder')}
          value={query}
        />
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto p-1">
        {turns.length === 0 ? (
          <div className="text-muted-foreground p-4 text-center text-xs">
            {t('turn_navigator_empty')}
          </div>
        ) : filtered.length === 0 ? (
          <div className="text-muted-foreground p-4 text-center text-xs">
            {t('turn_navigator_no_results')}
          </div>
        ) : (
          filtered.map((entryIx, row) => {
            const entry = turns[entryIx];
            return (
              <button
                className={cn(
                  'flex w-full items-center rounded px-2 py-1.5 text-left text-xs',
                  row === selected && 'bg-muted',
                )}
                key={entry.id}
                onClick={() => onNavigate(entry.id)}
                onMouseEnter={() => setSelected(row)}
                ref={(el) => {
                  rowRefs.current[row] = el;
                }}
                type="button"
              >
                <span className="min-w-0 flex-1 truncate">{entry.display}</span>
              </button>
            );
          })
        )}
      </div>
      <div className="border-t border-border px-2 py-1 text-center text-[10px] text-muted-foreground">
        {copied ? t('turn_navigator_copied') : '↑↓ · ↵ · ⌘C · Esc'}
      </div>
    </div>
  );
};
