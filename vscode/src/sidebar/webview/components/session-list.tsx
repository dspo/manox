// Session list column shared by the home screen and the three-column
// conversation layout. Rows show a status dot, the title, and the relative
// last-activity time, with pin/archive actions appearing on hover; archived
// rows collapse behind a "More" row. The active row is highlighted when the
// list sits beside an open conversation.

import {
  Archive,
  ArchiveRestore,
  ChevronRight,
  MessageSquare,
  Pin,
  ShipWheel,
  TriangleAlert,
} from 'lucide-react';
import { useEffect, useState } from 'react';

import type { ThreadListItem } from '../../../protocol';
import { api, ThreadApi } from '../api/client';
import { formatRelativeTime, t } from '../lib/i18n';
import { partitionSessions } from '../lib/sessions';
import { threadRowState } from '../lib/thread-status';
import { cn } from '../lib/utils';
import { store } from '../state/bridge';

export const openThread = (item: ThreadListItem) => {
  // Threads with live local state switch instantly and only refocus the
  // actor; the rest go through the host's open handshake.
  if (store.get().perThread[item.id]) {
    store.openLocal(item.id);
    new ThreadApi(item.id).focus();
  } else {
    api.openThread(item.id);
  }
};

const StatusIcon = ({ item }: { item: ThreadListItem }) => {
  switch (threadRowState(item)) {
    case 'errored':
      return <TriangleAlert className="text-danger size-3.5 shrink-0" />;
    case 'waiting':
    case 'unread':
      return <ShipWheel className="text-blue size-4 shrink-0" />;
    case 'autonomous':
      return <ShipWheel className="text-success size-4 shrink-0 animate-wheel-spin" />;
    case 'idle':
      return <ShipWheel className="text-foreground size-4 shrink-0" />;
  }
};

const RowActionButton = ({
  title,
  onClick,
  children,
}: {
  title: string;
  onClick: () => void;
  children: React.ReactNode;
}) => (
  <button
    className="text-muted-foreground hover:text-foreground cursor-pointer rounded p-1 transition-colors"
    onClick={(e) => {
      e.stopPropagation();
      onClick();
    }}
    title={title}
    type="button"
  >
    {children}
  </button>
);

const SessionRow = ({
  active,
  item,
  onOpen,
}: {
  active?: boolean;
  item: ThreadListItem;
  onOpen: (item: ThreadListItem) => void;
}) => (
  <li>
    <div
      className={cn(
        'group hover:bg-muted relative flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left',
        active && 'bg-muted',
      )}
      onClick={() => onOpen(item)}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          onOpen(item);
        }
      }}
      role="button"
      tabIndex={0}
    >
      <StatusIcon item={item} />
      <span className="min-w-0 flex-1 truncate text-sm">{item.title}</span>
      {item.pinned && !item.archived && (
        <Pin className="text-muted-foreground size-3 shrink-0" />
      )}
      <span className="text-muted-foreground shrink-0 text-xs">
        {formatRelativeTime(item.updated_at)}
      </span>
      <div
        className="bg-card absolute top-1/2 right-2 flex -translate-y-1/2 items-center rounded-md border opacity-0 shadow-sm transition-opacity group-focus-within:opacity-100 group-hover:opacity-100"
        onClick={(e) => e.stopPropagation()}
      >
        {item.archived ? (
          <RowActionButton onClick={() => api.archiveThread(item.id, false)} title={t('unarchive')}>
            <ArchiveRestore className="size-3.5" />
          </RowActionButton>
        ) : (
          <>
            <RowActionButton
              onClick={() => api.pinThread(item.id, !item.pinned)}
              title={item.pinned ? t('unpin') : t('pin')}
            >
              <Pin className={cn('size-3.5', item.pinned && 'text-info')} />
            </RowActionButton>
            <RowActionButton onClick={() => api.archiveThread(item.id, true)} title={t('archive')}>
              <Archive className="size-3.5" />
            </RowActionButton>
          </>
        )}
      </div>
    </div>
  </li>
);

export type SessionListProps = {
  threads: ThreadListItem[];
  /** Row highlighted when its session is the active conversation. */
  activeThreadId?: string | null;
  onOpen: (item: ThreadListItem) => void;
};

export const SessionList = ({ threads, activeThreadId, onOpen }: SessionListProps) => {
  const [archivedOpen, setArchivedOpen] = useState(false);

  // Relative times age on their own; tick once a minute to keep them honest.
  const [, setTick] = useState(0);
  useEffect(() => {
    const timer = setInterval(() => setTick((n) => n + 1), 60_000);
    return () => clearInterval(timer);
  }, []);

  const { active, archived } = partitionSessions(threads);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center gap-1 px-3 py-2">
        <span className="min-w-0 flex-1 text-[11px] font-bold uppercase tracking-wide">
          {t('sessions')}
        </span>
      </div>
      {active.length === 0 && archived.length === 0 ? (
        <div className="text-muted-foreground flex flex-1 flex-col items-center justify-center gap-2 text-sm">
          <MessageSquare className="size-6" />
          <p>{t('threads_empty')}</p>
        </div>
      ) : (
        <ul className="min-h-0 flex-1 overflow-y-auto py-1">
          {active.map((item) => (
            <SessionRow active={item.id === activeThreadId} item={item} key={item.id} onOpen={onOpen} />
          ))}
          {archived.length > 0 && (
            <>
              <li>
                <button
                  className="text-muted-foreground hover:bg-muted hover:text-foreground flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-xs transition-colors"
                  onClick={() => setArchivedOpen((open) => !open)}
                  type="button"
                >
                  <ChevronRight
                    className={cn('size-3.5 transition-transform', archivedOpen && 'rotate-90')}
                  />
                  <span>{t('more')}</span>
                  <span className="ml-auto">{archived.length}</span>
                </button>
              </li>
              {archivedOpen &&
                archived.map((item) => (
                  <SessionRow active={item.id === activeThreadId} item={item} key={item.id} onOpen={onOpen} />
                ))}
            </>
          )}
        </ul>
      )}
    </div>
  );
};
