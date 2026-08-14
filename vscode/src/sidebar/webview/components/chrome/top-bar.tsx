import { Plus } from 'lucide-react';
import type { ReactNode } from 'react';

import { api } from '../../api/client';
import { Badge } from '../ui/badge';
import { Button } from '../ui/button';

export type TopBarProps = {
  children?: ReactNode;
  approvalMode?: 'autopilot' | 'danger';
};

export const TopBar = ({ children, approvalMode }: TopBarProps) => (
  <div className="font-chrome flex items-center gap-1 border-b px-2 py-1.5">
    <div className="min-w-0 flex-1">{children}</div>
    {approvalMode === 'danger' && (
      <Badge className="shrink-0 px-1.5 text-[10px]" variant="destructive">
        danger
      </Badge>
    )}
    <Button
      onClick={() => api.newSession()}
      size="icon-sm"
      title="New session"
      variant="ghost"
    >
      <Plus className="size-4" />
    </Button>
  </div>
);
