// User message rendered as a gate frame: a rounded border tinted by the
// approval mode, opened at bottom center by a background-colored strip. The
// body renders as markdown, mirroring the gpui host's user-turn rendering.
// Parked submissions carry the queued lifecycle: a queued chip with hover
// actions (steer into the running turn / remove), a pending chip while a
// steer awaits injection, and a stranded-steer retry that resubmits the
// text as a plain message.

import { Loader2, RotateCw, Send, Trash2 } from 'lucide-react';
import { memo } from 'react';

import type { ApprovalMode } from '../../../../protocol';
import { ThreadApi } from '../../api/client';
import { t } from '../../lib/i18n';
import { cn } from '../../lib/utils';
import { store } from '../../state/bridge';
import type { TranscriptItem } from '../../state/store';
import { MarkdownContent } from '../ai/markdown-content';
import { CopyOnHover } from './copy-on-hover';

const timeFormat = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });

const actionClass =
  'text-muted-foreground hover:text-foreground flex cursor-pointer items-center gap-1 rounded-full border border-border px-1.5 py-0.5 text-[10px] transition-colors';

export type UserMessageProps = {
  item: Extract<TranscriptItem, { kind: 'user' }>;
  approvalMode: ApprovalMode;
  sessionId: string;
};

// Memoized: streaming frames re-render the whole transcript, but only the
// live items mutate — the queued/steer lifecycle lives on the item object.
export const UserMessage = memo(({ item, approvalMode, sessionId }: UserMessageProps) => {
  const time = item.timestamp ? timeFormat.format(item.timestamp * 1000) : null;
  const visibleImages = item.images?.filter((img) => img.data) ?? [];
  // The body is the persisted display form when present (non-empty), else
  // the full text — the host renders the same priority.
  const body = item.displayText || item.text;
  const chip = item.queued ? (
    <span className="text-muted-foreground inline-flex items-center rounded-full border border-border px-1.5 py-0.5 text-[10px]">
      {t('queued')}
    </span>
  ) : item.steerPendingId ? (
    <span className="text-info inline-flex items-center gap-1 rounded-full border border-border px-1.5 py-0.5 text-[10px]">
      <Loader2 className="size-2.5 animate-spin" />
      {t('steer_pending')}
    </span>
  ) : item.steerFailed ? (
    <span className="text-danger inline-flex items-center rounded-full border border-border px-1.5 py-0.5 text-[10px]">
      {t('steer_failed')}
    </span>
  ) : null;
  const actions = item.queued ? (
    <>
      <button
        className={actionClass}
        onClick={() => {
          store.markSteerPending(sessionId, item.clientId!);
          new ThreadApi(sessionId).steer(item.clientId!, item.text);
        }}
        title={t('steer_now')}
        type="button"
      >
        <Send className="size-2.5" />
        {t('steer_now')}
      </button>
      <button
        className={actionClass}
        onClick={() => {
          new ThreadApi(sessionId).dropQueued(item.clientId!);
          store.removeUser(sessionId, item.clientId!);
        }}
        title={t('drop_queued')}
        type="button"
      >
        <Trash2 className="size-2.5" />
        {t('drop_queued')}
      </button>
    </>
  ) : item.steerFailed ? (
    <button
      className={actionClass}
      onClick={() => {
        new ThreadApi(sessionId).submit(item.text);
        store.removeUser(sessionId, item.clientId!);
      }}
      title={t('steer_retry')}
      type="button"
    >
      <RotateCw className="size-2.5" />
      {t('steer_retry')}
    </button>
  ) : null;
  return (
    <div
      className={cn(
        'group relative rounded-xl border-2 px-4 pt-2 pb-3',
        approvalMode === 'danger-full-access' ? 'border-danger' : 'border-info',
      )}
    >
      <div className="bg-background pointer-events-none absolute bottom-[-2px] left-1/2 h-[2px] w-2/5 -translate-x-1/2" />
      <div className="text-muted-foreground mb-1 flex items-center gap-1.5 text-xs">
        <span className="text-foreground font-medium">{t('you')}</span>
        {time && (
          <>
            <span>›</span>
            <span>{time}</span>
          </>
        )}
        {item.modelId && (
          <>
            <span>›</span>
            <span className="truncate">{item.modelId}</span>
          </>
        )}
        <CopyOnHover className="ml-auto" text={item.text} />
      </div>
      {(chip || actions) && (
        <div className="mb-1 flex items-center gap-1.5">
          {chip}
          <div className="flex items-center gap-1.5 opacity-0 transition-opacity group-hover:opacity-100">
            {actions}
          </div>
        </div>
      )}
      <MarkdownContent content={body} />
      {visibleImages.length > 0 && (
        <div className="mt-2 flex flex-wrap gap-2">
          {visibleImages.map((img, index) => (
            <img
              alt={`attachment ${index + 1}`}
              className="h-16 w-16 rounded-md border object-cover"
              key={index}
              src={img.data ?? undefined}
            />
          ))}
        </div>
      )}
    </div>
  );
});
