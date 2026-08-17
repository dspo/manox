import { memo } from 'react';

import type { TranscriptItem } from '../../state/store';
import { MarkdownContent } from '../ai/markdown-content';
import { Message, MessageContent } from '../ai/message';
import { CopyOnHover } from './copy-on-hover';

export type AssistantMessageProps = {
  item: Extract<TranscriptItem, { kind: 'assistant' }>;
};

// Memoized: a streaming turn re-renders the transcript per frame, but only
// the trailing item mutates — finished items keep their object identity.
export const AssistantMessage = memo(({ item }: AssistantMessageProps) => (
  <Message className="relative" from="assistant">
    <CopyOnHover className="absolute top-0 right-0" text={item.text} />
    <MessageContent>
      <MarkdownContent content={item.text} />
    </MessageContent>
  </Message>
));
