import { CheckCircle, ChevronDown, Circle, Clock, Wrench, XCircle } from 'lucide-react';
import type { ComponentProps, ReactNode } from 'react';

import { cn } from '../../lib/utils';
import { Badge } from '../ui/badge';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '../ui/collapsible';

export type ToolStatus = 'pending' | 'running' | 'completed' | 'failed' | 'cancelled';

export type ToolProps = ComponentProps<typeof Collapsible>;

export const Tool = ({ className, ...props }: ToolProps) => (
  <Collapsible
    className={cn('group not-prose mb-4 w-full rounded-md border', className)}
    {...props}
  />
);

const statusLabels: Record<ToolStatus, string> = {
  pending: 'Pending',
  running: 'Running',
  completed: 'Completed',
  failed: 'Error',
  cancelled: 'Cancelled',
};

const statusIcons: Record<ToolStatus, ReactNode> = {
  pending: <Circle className="size-4" />,
  running: <Clock className="size-4 animate-pulse" />,
  completed: <CheckCircle className="size-4 text-green-600" />,
  failed: <XCircle className="size-4 text-red-600" />,
  cancelled: <Circle className="size-4 text-muted-foreground" />,
};

export const getStatusBadge = (status: string) => {
  if (status in statusLabels) {
    const known = status as ToolStatus;
    return (
      <Badge className="gap-1.5 rounded-full text-xs" variant="secondary">
        {statusIcons[known]}
        {statusLabels[known]}
      </Badge>
    );
  }
  return (
    <Badge className="gap-1.5 rounded-full text-xs" variant="outline">
      {status}
    </Badge>
  );
};

export type ToolHeaderProps = ComponentProps<typeof CollapsibleTrigger> & {
  title?: string;
  status: string;
};

export const ToolHeader = ({ className, title, status, ...props }: ToolHeaderProps) => (
  <CollapsibleTrigger
    className={cn('flex w-full items-center justify-between gap-4 p-3', className)}
    {...props}
  >
    <div className="flex min-w-0 items-center gap-2">
      <Wrench className="size-4 shrink-0 text-muted-foreground" />
      <span className="truncate font-medium text-sm">{title}</span>
      {getStatusBadge(status)}
    </div>
    <ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform group-data-[state=open]:rotate-180" />
  </CollapsibleTrigger>
);

export type ToolContentProps = ComponentProps<typeof CollapsibleContent>;

export const ToolContent = ({ className, ...props }: ToolContentProps) => (
  <CollapsibleContent
    className={cn(
      'data-[state=closed]:fade-out-0 data-[state=closed]:slide-out-to-top-2 data-[state=open]:slide-in-from-top-2 space-y-4 p-4 outline-none data-[state=closed]:animate-out data-[state=open]:animate-in',
      className,
    )}
    {...props}
  />
);

export type ToolOutputProps = ComponentProps<'div'> & {
  output?: string;
  errorText?: string;
};

export const ToolOutput = ({ className, output, errorText, ...props }: ToolOutputProps) => {
  if (!(output || errorText)) {
    return null;
  }

  return (
    <div className={cn('space-y-2', className)} {...props}>
      <h4 className="font-medium text-muted-foreground text-xs uppercase tracking-wide">
        {errorText ? 'Error' : 'Result'}
      </h4>
      <pre
        className={cn(
          'font-code max-h-[180px] overflow-auto whitespace-pre-wrap break-all rounded-md p-3 text-xs',
          errorText ? 'bg-destructive/10 text-destructive' : 'bg-muted/50 text-foreground',
        )}
      >
        {errorText ?? output}
      </pre>
    </div>
  );
};
