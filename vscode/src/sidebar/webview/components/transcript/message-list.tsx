// Transcript renderer. Items are grouped into turns — a user item opens a
// turn, everything after it belongs to that turn's reply. User messages
// carry their own gate frame; the assistant meta line (model · thinking ·
// tools · duration) sits directly above the first generated text of the
// turn, after any thinking/tool cards.

import type { ReactNode } from 'react';

import type { ApprovalMode, ModelInfo } from '../../../../protocol';
import { t } from '../../lib/i18n';
import type { TranscriptItem } from '../../state/store';
import {
  Conversation,
  ConversationContent,
  ConversationEmptyState,
  ConversationScrollButton,
} from '../ai/conversation';
import { ApprovalCard } from './approval-card';
import { AssistantMessage } from './assistant-message';
import { CopyOnHover } from './copy-on-hover';
import { ThinkingBlock } from './thinking-block';
import { ToolCallCard } from './tool-call-card';
import { UserMessage } from './user-message';

export type MessageListProps = {
  items: TranscriptItem[];
  turnActive: boolean;
  sessionId: string;
  models: ModelInfo[];
  approvalMode: ApprovalMode;
  /** Duration of the trailing finished turn; only the live session tracks
   * it, restored history leaves the meta line without a duration. */
  lastTurnDurationSec: number | null;
  cwd: string;
  branch: string | null;
};

interface TurnGroup {
  user: Extract<TranscriptItem, { kind: 'user' }> | null;
  rest: TranscriptItem[];
}

function groupIntoTurns(items: TranscriptItem[]): TurnGroup[] {
  const groups: TurnGroup[] = [];
  let current: TurnGroup | null = null;
  for (const item of items) {
    if (item.kind === 'user') {
      current = { user: item, rest: [] };
      groups.push(current);
    } else if (current) {
      current.rest.push(item);
    } else {
      // Reply items preceding any user message (restored-history edges).
      current = { user: null, rest: [item] };
      groups.push(current);
    }
  }
  return groups;
}

const modelName = (models: ModelInfo[], id?: string | null): string | null =>
  id ? (models.find((m) => m.id === id)?.name ?? id) : null;

export const MessageList = ({
  items,
  turnActive,
  sessionId,
  models,
  approvalMode,
  lastTurnDurationSec,
  cwd,
  branch,
}: MessageListProps) => {
  // Only the trailing item can be mid-stream.
  const lastItem = items[items.length - 1];
  const groups = groupIntoTurns(items);

  const renderItem = (item: TranscriptItem): ReactNode => {
    switch (item.kind) {
      case 'user':
        return <UserMessage approvalMode={approvalMode} item={item} key={item.id} />;
      case 'assistant':
        return <AssistantMessage item={item} key={item.id} />;
      case 'thinking':
        return (
          <ThinkingBlock
            isStreaming={turnActive && item === lastItem}
            key={item.id}
            text={item.text}
          />
        );
      case 'tool':
        return <ToolCallCard branch={branch} call={item.tool} cwd={cwd} key={item.id} />;
      case 'approval':
        return <ApprovalCard item={item} key={item.id} sessionId={sessionId} />;
      case 'compaction':
        return (
          <div className="text-center text-muted-foreground text-xs italic" key={item.id}>
            {t('context_compacted')}
          </div>
        );
    }
  };

  const renderMetaLine = (group: TurnGroup, index: number, isLastGroup: boolean) => {
    const assistantItems = group.rest.filter((i) => i.kind === 'assistant');
    if (assistantItems.length === 0) return null;
    const model = modelName(models, assistantItems[0].modelId);
    const thinkingCount = group.rest.filter((i) => i.kind === 'thinking').length;
    const toolCount = group.rest.filter((i) => i.kind === 'tool').length;
    const duration = isLastGroup && !turnActive ? lastTurnDurationSec : null;
    const parts: ReactNode[] = [];
    if (model) parts.push(<span className="text-primary" key="model">{model}</span>);
    if (thinkingCount > 0) parts.push(<span key="thinking">{t('thought_n_turns', thinkingCount)}</span>);
    if (toolCount > 0) parts.push(<span key="tools">{t('called_n_tools', toolCount)}</span>);
    if (duration !== null) parts.push(<span key="duration">{t('duration_seconds', duration)}</span>);
    const copyText = assistantItems
      .map((i) => (i.kind === 'assistant' ? i.text : ''))
      .join('\n\n');
    return (
      <div
        className="text-muted-foreground group mb-1 mt-3 flex items-center gap-1.5 px-1 text-xs"
        key={`meta-${index}`}
      >
        {parts.map((part, i) => (
          <FragmentNode key={i} node={part} separator={i > 0} />
        ))}
        <CopyOnHover className="ml-auto" text={copyText} />
      </div>
    );
  };

  const renderGroup = (group: TurnGroup, index: number) => {
    const nodes: ReactNode[] = [];
    if (group.user) {
      nodes.push(renderItem(group.user));
    }
    const metaLine = renderMetaLine(group, index, index === groups.length - 1);
    let metaInserted = false;
    for (const item of group.rest) {
      if (!metaInserted && item.kind === 'assistant' && metaLine) {
        nodes.push(metaLine);
        metaInserted = true;
      }
      nodes.push(renderItem(item));
    }
    return <div key={group.user?.id ?? `lead-${index}`}>{nodes}</div>;
  };

  return (
    <Conversation>
      <ConversationContent>
        <div className="mx-auto w-full max-w-[760px]">{groups.map(renderGroup)}</div>
      </ConversationContent>
      {items.length === 0 && (
        <ConversationEmptyState description={t('no_messages_desc')} title={t('no_messages_title')} />
      )}
      <ConversationScrollButton />
    </Conversation>
  );
};

const FragmentNode = ({ node, separator }: { node: ReactNode; separator: boolean }) => (
  <>
    {separator && <span>·</span>}
    {node}
  </>
);
