import type { TranscriptItem } from '../../state/store';
import { Message, MessageContent } from '../ai/message';

export type UserMessageProps = {
  item: Extract<TranscriptItem, { kind: 'user' }>;
};

export const UserMessage = ({ item }: UserMessageProps) => (
  <Message from="user">
    <MessageContent>
      <p className="whitespace-pre-wrap">{item.displayText ?? item.text}</p>
    </MessageContent>
  </Message>
);
