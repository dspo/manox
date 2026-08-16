// Home screen styled after the native chat sessions view. Rows show a
// status dot, the title, and the relative last-activity time, with
// pin/archive actions appearing on hover; archived rows collapse behind a
// "More" row. The composer creates a thread on its first send. Wide
// containers split into a sessions column and a composer column joined by a
// draggable sash — the composer pinned to the bottom of its column, like
// the workbench's side-by-side layout.

import {
  Archive,
  ArchiveRestore,
  ChevronRight,
  LoaderCircle,
  MessageSquare,
  Pin,
  TriangleAlert,
} from 'lucide-react';
import { useCallback, useEffect, useRef, useState } from 'react';

import type { CommandEntry, ModelInfo, ThreadListItem } from '../../../protocol';
import { api, ThreadApi } from '../api/client';
import { formatRelativeTime, t } from '../lib/i18n';
import { partitionSessions } from '../lib/sessions';
import { useContainerWidth } from '../lib/use-container-width';
import { cn } from '../lib/utils';
import { store } from '../state/bridge';
import { Composer } from './chrome/composer';
import { ErrorBanner } from './chrome/error-banner';

const WIDE_BREAKPOINT_PX = 600;
const SIDEBAR_MIN_PX = 200;
const SIDEBAR_DEFAULT_PX = 300;
const COMPOSER_COLUMN_MIN_PX = 300;
const SIDEBAR_WIDTH_KEY = 'manox.sessions-sidebar-width';
const DRAFT_MODEL_KEY = 'manox.draft-model-id';

const openThread = (item: ThreadListItem) => {
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
  if (item.running) {
    return <LoaderCircle className="text-info size-3.5 shrink-0 animate-spin" />;
  }
  if (item.pending_auth) {
    return <TriangleAlert className="text-warning size-3.5 shrink-0" />;
  }
  if (item.errored) {
    return <TriangleAlert className="text-danger size-3.5 shrink-0" />;
  }
  if (item.unread) {
    return <span className="bg-info block size-2 shrink-0 rounded-full" />;
  }
  return <span className="bg-muted-foreground/40 block size-1.5 shrink-0 rounded-full" />;
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

const SessionRow = ({ item }: { item: ThreadListItem }) => (
  <li>
    <div
      className="group hover:bg-muted relative flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left"
      onClick={() => openThread(item)}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          openThread(item);
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

export type ThreadsViewProps = {
  threads: ThreadListItem[];
  /** Global error surfaced above the list (per-thread errors stay in the
   * conversation view). */
  error: string | null;
  models: ModelInfo[];
  commands: CommandEntry[];
};

export const ThreadsView = ({ threads, error, models, commands }: ThreadsViewProps) => {
  const { ref, width } = useContainerWidth();
  const wide = width !== null && width >= WIDE_BREAKPOINT_PX;
  const [archivedOpen, setArchivedOpen] = useState(false);
  const [sidebarWidth, setSidebarWidth] = useState(() => {
    const saved = Number(localStorage.getItem(SIDEBAR_WIDTH_KEY));
    return saved >= SIDEBAR_MIN_PX ? saved : SIDEBAR_DEFAULT_PX;
  });
  const [sashActive, setSashActive] = useState(false);
  const sashDrag = useRef<{ startX: number; startWidth: number } | null>(null);
  const [draftModelId, setDraftModelId] = useState<string | null>(
    () => localStorage.getItem(DRAFT_MODEL_KEY) ?? null,
  );

  useEffect(() => {
    localStorage.setItem(SIDEBAR_WIDTH_KEY, String(sidebarWidth));
  }, [sidebarWidth]);

  const pickDraftModel = useCallback((modelId: string) => {
    setDraftModelId(modelId);
    localStorage.setItem(DRAFT_MODEL_KEY, modelId);
  }, []);

  // Relative times age on their own; tick once a minute to keep them honest.
  const [, setTick] = useState(0);
  useEffect(() => {
    const timer = setInterval(() => setTick((n) => n + 1), 60_000);
    return () => clearInterval(timer);
  }, []);

  const { active, archived } = partitionSessions(threads);

  const maxSidebar = Math.max(
    SIDEBAR_MIN_PX,
    (width ?? WIDE_BREAKPOINT_PX) - COMPOSER_COLUMN_MIN_PX,
  );
  const listWidth = Math.min(sidebarWidth, maxSidebar);

  const onSashPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    sashDrag.current = { startX: e.clientX, startWidth: listWidth };
    setSashActive(true);
    e.currentTarget.setPointerCapture(e.pointerId);
  };
  const onSashPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    if (!sashDrag.current) return;
    const next = sashDrag.current.startWidth + e.clientX - sashDrag.current.startX;
    setSidebarWidth(Math.max(SIDEBAR_MIN_PX, Math.min(next, maxSidebar)));
  };
  const onSashPointerUp = () => {
    sashDrag.current = null;
    setSashActive(false);
  };

  const createSession = useCallback(
    (text: string, images: { data: string; mimeType: string }[]) => {
      const id = crypto.randomUUID();
      store.draftThread(
        id,
        text,
        images.map((img) => ({
          mimeType: img.mimeType,
          data: `data:${img.mimeType};base64,${img.data}`,
          byteLen: null,
        })),
      );
      api.newSession({
        sessionId: id,
        text,
        images: images.length ? images : undefined,
        modelId: draftModelId ?? undefined,
      });
    },
    [draftModelId],
  );

  const composer = (
    <Composer
      approvalMode="autopilot"
      commands={commands}
      currentModelId={draftModelId}
      models={models}
      onCreateSession={createSession}
      onModelChange={pickDraftModel}
      planMode={false}
      sessionId={null}
      turnActive={false}
    />
  );

  const listPanel = (
    <>
      <div className="flex items-center gap-1 px-3 py-2">
        <span className="min-w-0 flex-1 text-[11px] font-bold uppercase tracking-wide">
          {t('sessions')}
        </span>
      </div>
      <ErrorBanner message={error} />
      {active.length === 0 && archived.length === 0 ? (
        <div className="text-muted-foreground flex flex-1 flex-col items-center justify-center gap-2 text-sm">
          <MessageSquare className="size-6" />
          <p>{t('threads_empty')}</p>
        </div>
      ) : (
        <ul className="min-h-0 flex-1 overflow-y-auto py-1">
          {active.map((item) => (
            <SessionRow item={item} key={item.id} />
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
                archived.map((item) => <SessionRow item={item} key={item.id} />)}
            </>
          )}
        </ul>
      )}
    </>
  );

  return (
    <div ref={ref} className="font-chrome flex h-screen flex-col bg-background text-foreground">
      {wide ? (
        <div className="flex min-h-0 flex-1">
          <div className="flex min-w-0 flex-col" style={{ width: listWidth }}>
            {listPanel}
          </div>
          <div
            aria-orientation="vertical"
            className="relative z-10 w-1 shrink-0 cursor-col-resize touch-none"
            onDoubleClick={() => setSidebarWidth(SIDEBAR_DEFAULT_PX)}
            onPointerDown={onSashPointerDown}
            onPointerMove={onSashPointerMove}
            onPointerUp={onSashPointerUp}
            role="separator"
          >
            <div
              className={cn(
                'bg-border absolute inset-y-0 left-1/2 w-px -translate-x-1/2 transition-colors',
                sashActive ? 'bg-ring' : 'hover:bg-ring',
              )}
            />
          </div>
          <div className="flex min-w-0 flex-1 flex-col justify-end">{composer}</div>
        </div>
      ) : (
        <>
          <div className="flex min-h-0 flex-1 flex-col">{listPanel}</div>
          {composer}
        </>
      )}
    </div>
  );
};
