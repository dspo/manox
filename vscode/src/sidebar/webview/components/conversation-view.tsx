// Conversation view: header with back arrow and thread title, transcript,
// error banner, composer, usage. A wide container additionally shows the
// conversation info panel.

import { ArrowLeft } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';

import type { CommandEntry, ModelInfo } from '../../../protocol';
import { api, ThreadApi } from '../api/client';
import type { ThreadState } from '../state/bridge';
import { store } from '../state/bridge';
import { Composer } from './chrome/composer';
import { ErrorBanner } from './chrome/error-banner';
import { UsageBar } from './chrome/usage-bar';
import { InfoPanel } from './info-panel';
import { MessageList } from './transcript/message-list';
import { Button } from './ui/button';

const WIDE_BREAKPOINT_PX = 560;

export type ConversationViewProps = {
  thread: ThreadState;
  models: ModelInfo[];
  commands: CommandEntry[];
  error: string | null;
};

export const ConversationView = ({ thread, models, commands, error }: ConversationViewProps) => {
  const containerRef = useRef<HTMLDivElement>(null);
  const [wide, setWide] = useState(false);

  // Container-width breakpoint: sidebar width depends on the panel layout,
  // not the viewport, so a ResizeObserver stands in for container queries.
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const observer = new ResizeObserver(() => {
      setWide(el.clientWidth >= WIDE_BREAKPOINT_PX);
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

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
        <Button onClick={backToList} size="icon-sm" title="Back to threads" variant="ghost">
          <ArrowLeft className="size-4" />
        </Button>
        <span className="min-w-0 flex-1 truncate font-medium text-sm">{thread.title}</span>
      </div>
      <div className="flex min-h-0 flex-1">
        <div className="flex min-w-0 flex-1 flex-col">
          <MessageList
            items={thread.items}
            models={models}
            sessionId={thread.sessionId}
            turnActive={thread.turnActive}
          />
        </div>
        {wide && <InfoPanel thread={thread} />}
      </div>
      <ErrorBanner message={error} />
      <Composer
        approvalMode={thread.approvalMode}
        commands={commands}
        currentModelId={thread.currentModelId}
        models={models}
        sessionId={thread.sessionId}
        turnActive={thread.turnActive}
      />
      <UsageBar usage={thread.usage} />
    </div>
  );
};
