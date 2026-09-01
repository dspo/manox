import { memo, useEffect, useRef, useState } from 'react';

import type { ToolCallState } from '../../state/store';
import { Tool, ToolContent, ToolHeader, ToolOutput } from '../ai/tool';
import { CopyOnHover } from './copy-on-hover';

const OUTPUT_CAP = 32_000;
const AUTO_CLOSE_DELAY = 1000;
const TERMINAL_STATUSES = new Set(['completed', 'failed', 'denied', 'cancelled', 'continued']);

// The store keeps a tail window of streamed output; clip again to what the
// card can render so a cap-sized string never enters the DOM whole.
function clip(text: string): string {
  if (text.length <= OUTPUT_CAP) {
    return text;
  }
  return `…${text.slice(-OUTPUT_CAP)}`;
}

export type ToolCallCardProps = {
  call: ToolCallState;
  cwd: string;
  branch: string | null;
};

// Open while the call is in flight, collapse once shortly after it reaches
// a terminal status; restored-history cards mount terminal and stay
// collapsed. After the one-shot auto-close the card is fully user-driven.
// Memoized: idle cards keep their `call` object identity across streaming
// frames; only the tool item receiving output re-renders.
export const ToolCallCard = memo(({ call, cwd, branch }: ToolCallCardProps) => {
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
  const dir = cwd.split('/').filter(Boolean).pop() ?? cwd;

  return (
    <div className="group relative">
      <CopyOnHover className="absolute top-0.5 right-7 z-10" text={output || call.title} />
      <Tool onOpenChange={handleOpenChange} open={open}>
        <ToolHeader autoApproved={call.autoApproved} status={call.status} title={call.title || call.name} />
        <ToolContent>
          {call.name === 'bash' && (
            <div className="font-code mb-1 text-xs italic">
              <span className="text-muted-foreground">
                {dir}
                {branch ? ` (${branch})` : ''}
              </span>{' '}
              <span className="text-[hsl(145,63%,47%)]">❯</span>{' '}
              <span className="text-foreground">{call.title || call.name}</span>
            </div>
          )}
          <ToolOutput errorText={errorText} output={errorText ? undefined : output} />
        </ToolContent>
      </Tool>
    </div>
  );
});
