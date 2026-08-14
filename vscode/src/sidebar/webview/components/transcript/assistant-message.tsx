import { MarkdownContent } from '../ai/markdown-content';
import { Message, MessageContent } from '../ai/message';

export type AssistantMessageProps = {
  text: string;
};

export const AssistantMessage = ({ text }: AssistantMessageProps) => (
  <Message from="assistant">
    <MessageContent>
      <MarkdownContent content={text} />
    </MessageContent>
  </Message>
);
