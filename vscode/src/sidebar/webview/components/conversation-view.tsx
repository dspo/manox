// Conversation view: header, transcript, error banner, and the composer
// pinned beneath the transcript. The layout widens in steps with the
// container: the conversation alone, then the conversation info panel
// joins as a side column, then the session list joins on the left.

import { ArrowLeft } from 'lucide-react';
import { useEffect } from 'react';

import type { CommandEntry, ModelInfo, ThreadListItem } from '../../../protocol';
import { api, ThreadApi } from '../api/client';
import { t } from '../lib/i18n';
import { chatLayoutForWidth, INFO_PANEL_WIDTH_PX, maxSessionListWidth } from '../lib/layout';
import { useContainerWidth } from '../lib/use-container-width';
import type { ThreadState } from '../state/bridge';
import { store } from '../state/bridge';
import { Composer } from './chrome/composer';
import { PlanModeBanner } from './chrome/plan-mode-banner';
import { ErrorBanner } from './chrome/error-banner';
import { InfoPanel } from './info-panel';
import { openThread, SessionList } from './session-list';
import { SidebarSash, SIDEBAR_MIN_PX, useSidebarWidth } from './sidebar-sash';
import { MessageList } from './transcript/message-list';
import { Button } from './ui/button';

export type ConversationViewProps = {
  thread: ThreadState;
  threads: ThreadListItem[];
  models: ModelInfo[];
  commands: CommandEntry[];
  error: string | null;
};

export const ConversationView = ({
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

  // Restore the info snapshot whenever a thread comes into view; live
  // plan/worktree/sub-agent events keep it fresh afterwards.
  useEffect(() => {
    const threadApi = new ThreadApi(thread.sessionId);
    threadApi.requestThreadInfo();
    threadApi.requestUsage();
  }, [thread.sessionId]);

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
            <MessageList
              approvalMode={thread.approvalMode}
              backgroundTasks={thread.backgroundTasks}
              branch={thread.branch}
              cwd={thread.cwd}
              items={thread.items}
              lastTurnDurationSec={thread.lastTurnDurationSec}
              models={models}
              sessionId={thread.sessionId}
              turnActive={thread.turnActive}
            />
          </div>
          <ErrorBanner message={error} />
          <Composer
            approvalMode={thread.approvalMode}
            commands={commands}
            creating={store.isCreating(thread.sessionId)}
            currentModelId={thread.currentModelId}
            models={models}
            planMode={thread.planMode}
            reasoningEffort={thread.reasoningEffort}
            sessionId={thread.sessionId}
            turnActive={thread.turnActive}
          />
        </div>
        {layout !== 'conversation' && (
          <div
            className="flex shrink-0 flex-col border-l border-border p-2"
            style={{ width: INFO_PANEL_WIDTH_PX }}
          >
            <InfoPanel className="min-h-0 flex-1 overflow-y-auto" models={models} thread={thread} />
          </div>
        )}
      </div>
    </div>
  );
};
