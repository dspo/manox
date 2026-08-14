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
};

// The transcript is append-only, so index keys stay stable for the whole
// session; a session reset swaps the array and remounts everything.
export const MessageList = ({ items, turnActive }: MessageListProps) => (
  <Conversation>
    <ConversationContent>
      {items.map((item, index) => {
        switch (item.kind) {
          case 'user':
            return <UserMessage key={index} text={item.text} />;
          case 'assistant':
            return <AssistantMessage key={index} text={item.text} />;
          case 'thinking':
            return (
              <ThinkingBlock
                isStreaming={turnActive && index === items.length - 1}
                key={index}
                text={item.text}
              />
            );
          case 'tool':
            return <ToolCallCard call={item.call} key={item.call.id} />;
          case 'approval':
            return <ApprovalCard item={item} key={item.id} />;
        }
      })}
    </ConversationContent>
    {items.length === 0 && (
      <ConversationEmptyState description="Send a message to start" title="No messages yet" />
    )}
    <ConversationScrollButton />
  </Conversation>
);
