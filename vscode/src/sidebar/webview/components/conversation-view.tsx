// Conversation view: header, transcript, error banner, and the composer
// pinned beneath the transcript. Once the container is wide enough the
// conversation info card floats over the transcript's top-right corner;
// the transcript keeps a matching right gutter so messages clear the card,
// mirroring the gpui host's context rail.

import { ArrowLeft } from 'lucide-react';
import { useEffect } from 'react';

import type { CommandEntry, ModelInfo } from '../../../protocol';
import { api, ThreadApi } from '../api/client';
import { t } from '../lib/i18n';
import { useContainerWidth } from '../lib/use-container-width';
import type { ThreadState } from '../state/bridge';
import { store } from '../state/bridge';
import { Composer } from './chrome/composer';
import { ErrorBanner } from './chrome/error-banner';
import { InfoPanel } from './info-panel';
import { MessageList } from './transcript/message-list';
import { Button } from './ui/button';

const WIDE_BREAKPOINT_PX = 760;
const INFO_CARD_WIDTH_PX = 260;
// Right gutter the transcript reserves so messages clear the floating card
// and its shadow.
const INFO_GUTTER_PX = INFO_CARD_WIDTH_PX + 36;

export type ConversationViewProps = {
  thread: ThreadState;
  models: ModelInfo[];
  commands: CommandEntry[];
  error: string | null;
};

export const ConversationView = ({ thread, models, commands, error }: ConversationViewProps) => {
  const { ref: containerRef, width } = useContainerWidth();
  const wide = width !== null && width >= WIDE_BREAKPOINT_PX;

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
        <Button onClick={backToList} size="icon-sm" title={t('back_to_threads')} variant="ghost">
          <ArrowLeft className="size-4" />
        </Button>
        <span className="min-w-0 flex-1 truncate font-medium text-sm">{thread.title}</span>
      </div>
      <div className="relative flex min-h-0 flex-1 flex-col">
        <MessageList
          approvalMode={thread.approvalMode}
          branch={thread.branch}
          cwd={thread.cwd}
          items={thread.items}
          lastTurnDurationSec={thread.lastTurnDurationSec}
          models={models}
          rightInsetPx={wide ? INFO_GUTTER_PX : undefined}
          sessionId={thread.sessionId}
          turnActive={thread.turnActive}
        />
        {wide && (
          <div className="pointer-events-none absolute inset-y-4 right-4 flex w-[260px] flex-col">
            <InfoPanel
              className="pointer-events-auto max-h-full overflow-y-auto"
              models={models}
              thread={thread}
            />
          </div>
        )}
      </div>
      <ErrorBanner message={error} />
      <Composer
        approvalMode={thread.approvalMode}
        commands={commands}
        creating={store.isCreating(thread.sessionId)}
        currentModelId={thread.currentModelId}
        models={models}
        planMode={thread.planMode}
        sessionId={thread.sessionId}
        turnActive={thread.turnActive}
      />
    </div>
  );
};
