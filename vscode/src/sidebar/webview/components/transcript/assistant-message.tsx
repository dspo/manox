import type { TranscriptItem } from '../../state/store';
import { MarkdownContent } from '../ai/markdown-content';
import { Message, MessageContent } from '../ai/message';

export type AssistantMessageProps = {
  item: Extract<TranscriptItem, { kind: 'assistant' }>;
};

export const AssistantMessage = ({ item }: AssistantMessageProps) => (
  <Message from="assistant">
    <MessageContent>
      <MarkdownContent content={item.text} />
    </MessageContent>
  </Message>
);
