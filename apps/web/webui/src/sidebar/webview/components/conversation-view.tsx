// Conversation view: header, transcript, error banner, and the composer
// pinned beneath the transcript. The layout widens in steps with the
// container: the conversation alone, then the conversation info card
// floats over the transcript, then the session list joins on the left.

import { ArrowLeft, Search } from 'lucide-react';
import { memo, useEffect, useMemo, useRef, useState } from 'react';

import type { CommandEntry, ModelInfo, ThreadListItem } from '../../../protocol';
import { api, onOpenTurnNavigator, ThreadApi } from '../api/client';
import { t } from '../lib/i18n';
import { chatLayoutForWidth, INFO_CARD_GUTTER_PX, INFO_CARD_WIDTH_PX, maxSessionListWidth } from '../lib/layout';
import { collectUserTurns } from '../lib/turn-nav';
import { useContainerWidth } from '../lib/use-container-width';
import type { ThreadState, TranscriptItem } from '../state/bridge';
import { store } from '../state/bridge';
import { Composer } from './chrome/composer';
import { PlanModeBanner } from './chrome/plan-mode-banner';
import { ErrorBanner } from './chrome/error-banner';
import { InfoPanel } from './info-panel';
import { openThread, SessionList } from './session-list';
import { SidebarSash, SIDEBAR_MIN_PX, useSidebarWidth } from './sidebar-sash';
import { MessageList } from './transcript/message-list';
import { TurnNavigator } from './turn-navigator';
import { Button } from './ui/button';

export type ConversationViewProps = {
  thread: ThreadState;
  threads: ThreadListItem[];
  models: ModelInfo[];
  commands: CommandEntry[];
  error: string | null;
};

// Memoized on the active thread's state reference: events folded for other
// sessions replace the per-thread map but never this ThreadState object, so
// concurrent streaming elsewhere never reconciles this view.
export const ConversationView = memo(({
  thread,
  threads,
  models,
  commands,
  error,
}: ConversationViewProps) => {
  const { ref: containerRef, width } = useContainerWidth();
  const layout = chatLayoutForWidth(width);
  // The left list column only exists in the three-column layout; its drag
  // range keeps the conversation column above its non-cramped minimum.
  const { width: listWidth, ...sash } = useSidebarWidth(
    Math.max(SIDEBAR_MIN_PX, maxSessionListWidth(width)),
  );

  // Backwards-paging affordance: true while records exist before the
  // published window head (§D.2 PageHistory).
  const hasMore = store.hasMoreHistory(thread.sessionId);
  // The conversation-info pull (§E.3 Q face) is store-driven: the
  // committed-message edge from the journal window schedules the debounced
  // `GetConversationInfo`; no per-view request effects any more.
  const [navigatorOpen, setNavigatorOpen] = useState(false);
  const composerInputRef = useRef<HTMLTextAreaElement | null>(null);
  // macOS cmd+m arrives from the host (VS Code keybinding command) and
  // toggles the navigator, mirroring the gpui host's global binding.
  useEffect(() => onOpenTurnNavigator(() => setNavigatorOpen((open) => !open)), []);
  // The collect pass only matters while the overlay is open; the transcript
  // streams a new item reference on every token during a turn.
  const turns = useMemo(
    () => (navigatorOpen ? collectUserTurns(thread.items) : []),
    [navigatorOpen, thread.items],
  );
  // Recall texts stay available while the composer is live; the overlay's
  // perf gate above does not apply to them because recall must work without
  // opening the navigator first.
  const userTurns = useMemo(
    () =>
      thread.items
        .filter(
          (item): item is Extract<TranscriptItem, { kind: 'user' }> =>
            item.kind === 'user' && item.text.trim() !== '',
        )
        .map((item) => ({ id: item.id, text: item.text }))
        .reverse(),
    [thread.items],
  );

  const closeNavigator = () => {
    setNavigatorOpen(false);
    composerInputRef.current?.focus();
  };

  const navigateToTurn = (id: string) => {
    closeNavigator();
    document.getElementById(`turn-${id}`)?.scrollIntoView({ behavior: 'smooth', block: 'start' });
  };

  const backToList = () => {
    store.backToList();
    api.blurThread();
  };

  return (
    <div ref={containerRef} className="font-chrome flex h-screen flex-col bg-background text-foreground">
      <div className="flex items-center gap-1 border-b px-2 py-1.5">
        {layout !== 'list-conversation-info' && (
          <Button onClick={backToList} size="icon-sm" title={t('back_to_threads')} variant="ghost">
            <ArrowLeft className="size-4" />
          </Button>
        )}
        <Button
          onClick={() => setNavigatorOpen((open) => !open)}
          size="icon-sm"
          title={t('turn_navigator_title')}
          variant="ghost"
        >
          <Search className="size-4" />
        </Button>
        <span className="min-w-0 flex-1 truncate font-medium text-sm">{thread.title}</span>
      </div>
      {thread.planMode && <PlanModeBanner sessionId={thread.sessionId} />}
      <div className="flex min-h-0 flex-1">
        {layout === 'list-conversation-info' && (
          <>
            <div className="flex min-w-0 flex-col" style={{ width: listWidth }}>
              <SessionList
                activeThreadId={thread.sessionId}
                onOpen={openThread}
                threads={threads}
              />
            </div>
            <SidebarSash {...sash} />
          </>
        )}
        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          <div className="relative flex min-h-0 flex-1 flex-col">
            {/* §D.2 PageHistory: backwards paging through the engine's
             * prepend data source. Shown while older records exist before
             * the published window head. */}
            {hasMore && (
              <div className="flex justify-center py-1">
                <button
                  className="text-muted-foreground hover:text-foreground cursor-pointer rounded-full border border-border px-2.5 py-0.5 text-xs transition-colors"
                  onClick={() => void store.requestOlder(thread.sessionId)}
                  type="button"
                >
                  {t('load_older')}
                </button>
              </div>
            )}
            <MessageList
              approvalMode={thread.approvalMode}
              backgroundTasks={thread.backgroundTasks}
              branch={thread.branch}
              cwd={thread.cwd}
              items={thread.items}
              lastTurnDurationSec={thread.lastTurnDurationSec}
              models={models}
              rightInsetPx={layout !== 'conversation' ? INFO_CARD_GUTTER_PX : undefined}
              sessionId={thread.sessionId}
              turnActive={thread.turnActive}
            />
            {layout !== 'conversation' && (
              <div
                className="pointer-events-none absolute inset-y-4 right-4 flex flex-col"
                style={{ width: INFO_CARD_WIDTH_PX }}
              >
                <InfoPanel
                  className="pointer-events-auto max-h-full overflow-y-auto"
                  models={models}
                  thread={thread}
                />
              </div>
            )}
            {navigatorOpen && (
              <div
                className="absolute inset-0 z-10 flex items-center justify-center bg-background/60"
                onClick={closeNavigator}
              >
                <TurnNavigator onClose={closeNavigator} onNavigate={navigateToTurn} turns={turns} />
              </div>
            )}
          </div>
          <ErrorBanner message={error} />
          <Composer
            approvalMode={thread.approvalMode}
            commands={commands}
            composerInputRef={composerInputRef}
            creating={store.isCreating(thread.sessionId)}
            currentModelRef={thread.modelRef}
            models={models}
            onOpenTurnNavigator={() => setNavigatorOpen(true)}
            planMode={thread.planMode}
            reasoningEffort={thread.reasoningEffort}
            sessionId={thread.sessionId}
            turnActive={thread.turnActive}
            userTurns={userTurns}
          />
        </div>
      </div>
    </div>
  );
});
