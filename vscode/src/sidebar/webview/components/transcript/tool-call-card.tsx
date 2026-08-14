import { useEffect, useRef, useState } from 'react';

import type { ToolCallState } from '../../state/store';
import { Tool, ToolContent, ToolHeader, ToolOutput } from '../ai/tool';

const OUTPUT_CAP = 32_000;
const AUTO_CLOSE_DELAY = 1000;
const TERMINAL_STATUSES = new Set(['completed', 'failed', 'denied', 'cancelled', 'continued']);

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

// Open while the call is in flight, collapse once shortly after it reaches
// a terminal status; restored-history cards mount terminal and stay
// collapsed. After the one-shot auto-close the card is fully user-driven.
export const ToolCallCard = ({ call }: ToolCallCardProps) => {
  const isTerminal = TERMINAL_STATUSES.has(call.status);
  const [open, setOpen] = useState(!isTerminal);
  const [hasAutoClosed, setHasAutoClosed] = useState(false);
  const userTouchedRef = useRef(false);

  useEffect(() => {
    if (isTerminal && open && !hasAutoClosed && !userTouchedRef.current) {
      const timer = setTimeout(() => {
        setOpen(false);
        setHasAutoClosed(true);
      }, AUTO_CLOSE_DELAY);
      return () => clearTimeout(timer);
    }
  }, [isTerminal, open, hasAutoClosed]);

  const handleOpenChange = (next: boolean) => {
    userTouchedRef.current = true;
    setOpen(next);
  };

  const output = clip(call.output);
  const errorText = call.isError ? output : undefined;

  return (
    <Tool onOpenChange={handleOpenChange} open={open}>
      <ToolHeader status={call.status} title={call.title || call.name} />
      <ToolContent>
        <ToolOutput errorText={errorText} output={output} />
      </ToolContent>
    </Tool>
  );
};
