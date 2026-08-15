import { t } from '../../lib/i18n';
import { Reasoning, ReasoningContent, ReasoningTrigger } from '../ai/reasoning';
import { Shimmer } from '../ai/shimmer';
import { CopyOnHover } from './copy-on-hover';

export type ThinkingBlockProps = {
  text: string;
  isStreaming: boolean;
};

export const ThinkingBlock = ({ text, isStreaming }: ThinkingBlockProps) => (
  <div className="group relative">
    <CopyOnHover className="absolute top-0 right-0" text={text} />
    <Reasoning isStreaming={isStreaming}>
      <ReasoningTrigger
        getThinkingMessage={(streaming, duration) => {
          if (streaming || duration === 0) {
            return <Shimmer duration={1}>{t('thinking')}</Shimmer>;
          }
          if (duration === undefined) {
            return <p>{t('thought_brief')}</p>;
          }
          return <p>{t('thought_seconds', duration)}</p>;
        }}
      />
      <ReasoningContent>{text}</ReasoningContent>
    </Reasoning>
  </div>
);
