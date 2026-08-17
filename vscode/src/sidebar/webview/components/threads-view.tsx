// Home screen styled after the native chat sessions view. Wide containers
// split into a sessions column and a composer column joined by a draggable
// sash — the composer pinned to the bottom of its column, like the
// workbench's side-by-side layout.

import { useCallback, useState } from 'react';

import type { CommandEntry, ModelInfo, ThreadListItem } from '../../../protocol';
import { api } from '../api/client';
import { useContainerWidth } from '../lib/use-container-width';
import { store } from '../state/bridge';
import { Composer } from './chrome/composer';
import { ErrorBanner } from './chrome/error-banner';
import { openThread, SessionList } from './session-list';
import { SidebarSash, SIDEBAR_MIN_PX, useSidebarWidth } from './sidebar-sash';

const WIDE_BREAKPOINT_PX = 600;
const COMPOSER_COLUMN_MIN_PX = 300;
const DRAFT_MODEL_KEY = 'manox.draft-model-id';

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
  const [draftModelId, setDraftModelId] = useState<string | null>(
    () => localStorage.getItem(DRAFT_MODEL_KEY) ?? null,
  );
  const { width: listWidth, ...sash } = useSidebarWidth(
    Math.max(SIDEBAR_MIN_PX, (width ?? WIDE_BREAKPOINT_PX) - COMPOSER_COLUMN_MIN_PX),
  );

  const pickDraftModel = useCallback((modelId: string) => {
    setDraftModelId(modelId);
    localStorage.setItem(DRAFT_MODEL_KEY, modelId);
  }, []);

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
      reasoningEffort="high"
      sessionId={null}
      turnActive={false}
    />
  );

  const list = (
    <>
      <ErrorBanner message={error} />
      <SessionList threads={threads} onOpen={openThread} />
    </>
  );

  return (
    <div ref={ref} className="font-chrome flex h-screen flex-col bg-background text-foreground">
      {wide ? (
        <div className="flex min-h-0 flex-1">
          <div className="flex min-w-0 flex-col" style={{ width: listWidth }}>
            {list}
          </div>
          <SidebarSash {...sash} />
          <div className="flex min-w-0 flex-1 flex-col justify-end">{composer}</div>
        </div>
      ) : (
        <>
          <div className="flex min-h-0 flex-1 flex-col">{list}</div>
          {composer}
        </>
      )}
    </div>
  );
};
