import type { ToolCallState } from '../../state/store';
import { Tool, ToolContent, ToolHeader, ToolOutput } from '../ai/tool';

const OUTPUT_CAP = 32_000;

// Streaming output grows unbounded; keep only the tail for display.
function clip(text: string): string {
  if (text.length <= OUTPUT_CAP) {
    return text;
  }
  return `…${text.slice(-OUTPUT_CAP)}`;
}

export type ToolCallCardProps = {
  call: ToolCallState;
};

export const ToolCallCard = ({ call }: ToolCallCardProps) => {
  const errorText = call.result?.isError ? clip(call.result.output) : undefined;
  const output = call.result ? clip(call.result.output) : clip(call.output);

  return (
    <Tool defaultOpen>
      <ToolHeader status={call.status} title={call.title || call.name} />
      <ToolContent>
        <ToolOutput errorText={errorText} output={output} />
      </ToolContent>
    </Tool>
  );
};
