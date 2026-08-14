import { Message, MessageContent } from '../ai/message';

export type UserMessageProps = {
  text: string;
};

export const UserMessage = ({ text }: UserMessageProps) => (
  <Message from="user">
    <MessageContent>
      <p className="whitespace-pre-wrap">{text}</p>
    </MessageContent>
  </Message>
);
