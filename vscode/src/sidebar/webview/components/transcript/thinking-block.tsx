import { Reasoning, ReasoningContent, ReasoningTrigger } from '../ai/reasoning';

export type ThinkingBlockProps = {
  text: string;
  isStreaming: boolean;
};

export const ThinkingBlock = ({ text, isStreaming }: ThinkingBlockProps) => (
  <Reasoning isStreaming={isStreaming}>
    <ReasoningTrigger />
    <ReasoningContent>{text}</ReasoningContent>
  </Reasoning>
);
