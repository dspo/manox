import { Ban, Check, ChevronRight, Circle, Clock, MinusCircle, X } from 'lucide-react';
import type { ComponentProps, ReactNode } from 'react';
import { useState } from 'react';

import { t } from '../../lib/i18n';
import { cn } from '../../lib/utils';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '../ui/collapsible';
import { BrailleSpinner } from '../ui/braille-spinner';

/** UI status vocabulary. Terminal wire statuses are folded in the store
 * (success → completed, error → failed); authorization states pass through. */
export type ToolStatus =
  | 'pending-approval'
  | 'running'
  | 'completed'
  | 'failed'
  | 'denied'
  | 'cancelled'
  | 'continued';

export type ToolProps = ComponentProps<typeof Collapsible>;

export const Tool = ({ className, ...props }: ToolProps) => (
  <Collapsible
    className={cn('group not-prose mb-4 w-full overflow-hidden rounded-lg border border-border/50', className)}
    {...props}
  />
);


const statusIcons: Record<ToolStatus, ReactNode> = {
  'pending-approval': <Clock className="size-3.5 shrink-0 text-warning" />,
  running: <BrailleSpinner className="text-muted-foreground" />,
  completed: <Check className="size-3.5 shrink-0 text-success" />,
  failed: <X className="size-3.5 shrink-0 text-danger" />,
  denied: <Ban className="size-3.5 shrink-0 text-warning" />,
  cancelled: <Circle className="size-3.5 shrink-0 text-muted-foreground" />,
  continued: <MinusCircle className="size-3.5 shrink-0 text-muted-foreground" />,
};

export type ToolHeaderProps = ComponentProps<typeof CollapsibleTrigger> & {
  title?: string;
  status: string;
};

export const ToolHeader = ({ className, title, status, ...props }: ToolHeaderProps) => (
  <CollapsibleTrigger
    className={cn(
      'font-code text-muted-foreground hover:bg-accent/50 flex w-full cursor-pointer items-center gap-1.5 px-2 py-1 text-[13px] italic transition-colors',
      className,
    )}
    {...props}
  >
    <ChevronRight className="size-3.5 shrink-0 transition-transform group-data-[state=open]:rotate-90" />
    <span className="min-w-0 flex-1 truncate text-left">{title}</span>
    {(statusIcons as Record<string, ReactNode>)[status] ?? null}
  </CollapsibleTrigger>
);

export type ToolContentProps = ComponentProps<typeof CollapsibleContent>;

export const ToolContent = ({ className, ...props }: ToolContentProps) => (
  <CollapsibleContent
    className={cn(
      'data-[state=closed]:fade-out-0 data-[state=closed]:slide-out-to-top-2 data-[state=open]:slide-in-from-top-2 px-3 py-2 outline-none data-[state=closed]:animate-out data-[state=open]:animate-in',
      className,
    )}
    {...props}
  />
);

const MAX_VISIBLE_LINES = 20;

const lineClass = (line: string, isError: boolean): string => {
  if (isError) return 'text-danger whitespace-pre-wrap break-all';
  if (line.startsWith('+')) return 'text-success whitespace-pre-wrap break-all';
  if (line.startsWith('-')) return 'text-danger whitespace-pre-wrap break-all';
  return 'text-muted-foreground whitespace-pre-wrap break-all';
};

export type ToolOutputProps = ComponentProps<'div'> & {
  output?: string;
  errorText?: string;
};

export const ToolOutput = ({ className, output, errorText, ...props }: ToolOutputProps) => {
  const [expanded, setExpanded] = useState(false);
  const text = errorText ?? output;
  if (!text) return null;
  const lines = text.replace(/\n$/, '').split('\n');
  const hidden = lines.length - MAX_VISIBLE_LINES;
  const visible = hidden <= 0 || expanded ? lines : lines.slice(0, MAX_VISIBLE_LINES);
  return (
    <div className={cn('font-code text-xs italic', className)} {...props}>
      {visible.map((line, index) => (
        <div className={lineClass(line, Boolean(errorText))} key={index}>
          {line || ' '}
        </div>
      ))}
      {hidden > 0 && !expanded && (
        <button
          className="text-primary cursor-pointer not-italic"
          onClick={() => setExpanded(true)}
          type="button"
        >
          {t('show_n_more', hidden)}
        </button>
      )}
    </div>
  );
};
