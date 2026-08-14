// Transcript renderer. Items are grouped into turns — a user item opens a
// turn, everything after it belongs to that turn's reply — and each part
// gets a small title row: "You · model · time" for the user message,
// "model · N thinking · M tools" for the assistant segment.

import type { ReactNode } from 'react';

import type { ModelInfo } from '../../../../protocol';
import type { TranscriptItem } from '../../state/store';
import {
  Conversation,
  ConversationContent,
  ConversationEmptyState,
  ConversationScrollButton,
} from '../ai/conversation';
import { ApprovalCard } from './approval-card';
import { AssistantMessage } from './assistant-message';
import { ThinkingBlock } from './thinking-block';
import { ToolCallCard } from './tool-call-card';
import { UserMessage } from './user-message';

export type MessageListProps = {
  items: TranscriptItem[];
  turnActive: boolean;
  sessionId: string;
  models: ModelInfo[];
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

const timeFormat = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });

const formatTime = (unixSeconds?: number | null): string | null =>
  unixSeconds ? timeFormat.format(unixSeconds * 1000) : null;

const modelName = (models: ModelInfo[], id?: string | null): string | null =>
  id ? (models.find((m) => m.id === id)?.name ?? id) : null;

const TurnTitle = ({ children }: { children: ReactNode }) => (
  <div className="text-muted-foreground mt-4 mb-1 flex items-center gap-1.5 px-1 text-[11px] first:mt-0">
    {children}
  </div>
);

const countLabel = (count: number, singular: string, plural: string): string | null =>
  count === 0 ? null : `${count} ${count === 1 ? singular : plural}`;

export const MessageList = ({ items, turnActive, sessionId, models }: MessageListProps) => {
  // Only the trailing item can be mid-stream.
  const lastItem = items[items.length - 1];

  const renderItem = (item: TranscriptItem) => {
    switch (item.kind) {
      case 'user':
        return <UserMessage item={item} key={item.id} />;
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
        return <ToolCallCard call={item.tool} key={item.id} />;
      case 'approval':
        return <ApprovalCard item={item} key={item.id} sessionId={sessionId} />;
      case 'compaction':
        return (
          <div className="text-center text-muted-foreground text-xs italic" key={item.id}>
            context compacted
          </div>
        );
    }
  };

  const renderGroup = (group: TurnGroup, index: number) => {
    const nodes: ReactNode[] = [];
    if (group.user) {
      const time = formatTime(group.user.timestamp);
      const model = modelName(models, group.user.modelId);
      nodes.push(
        <TurnTitle key={`title-${group.user.id}`}>
          <span className="text-foreground font-medium">You</span>
          {model && <span>· {model}</span>}
          {time && <span>· {time}</span>}
        </TurnTitle>,
      );
      nodes.push(renderItem(group.user));
    }
    if (group.rest.length > 0) {
      const assistant = group.rest.find((i) => i.kind === 'assistant');
      const model = modelName(
        models,
        assistant && assistant.kind === 'assistant' ? assistant.modelId : null,
      );
      const thinkingCount = group.rest.filter((i) => i.kind === 'thinking').length;
      const toolCount = group.rest.filter((i) => i.kind === 'tool').length;
      const parts = [
        model,
        countLabel(thinkingCount, 'thinking round', 'thinking rounds'),
        countLabel(toolCount, 'tool call', 'tool calls'),
      ].filter(Boolean);
      if (parts.length > 0) {
        nodes.push(
          <TurnTitle key={`title-assistant-${index}`}>{parts.join(' · ')}</TurnTitle>,
        );
      }
      nodes.push(...group.rest.map(renderItem));
    }
    return <div key={group.user?.id ?? `lead-${index}`}>{nodes}</div>;
  };

  return (
    <Conversation>
      <ConversationContent>{groupIntoTurns(items).map(renderGroup)}</ConversationContent>
      {items.length === 0 && (
        <ConversationEmptyState description="Send a message to start" title="No messages yet" />
      )}
      <ConversationScrollButton />
    </Conversation>
  );
};
