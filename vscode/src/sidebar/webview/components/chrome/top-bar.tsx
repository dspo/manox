import { Plus } from 'lucide-react';
import type { ReactNode } from 'react';

import { api } from '../../api/client';
import { Button } from '../ui/button';

export type TopBarProps = {
  children?: ReactNode;
};

export const TopBar = ({ children }: TopBarProps) => (
  <div className="font-chrome flex items-center gap-1 border-b px-2 py-1.5">
    <div className="min-w-0 flex-1">{children}</div>
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
