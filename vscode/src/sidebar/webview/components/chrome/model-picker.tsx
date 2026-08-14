import { Check, ChevronsUpDown } from 'lucide-react';

import { ThreadApi } from '../../api/client';
import type { ModelInfo } from '../../../../protocol';
import { Button } from '../ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '../ui/dropdown-menu';

export type ModelPickerProps = {
  models: ModelInfo[];
  currentModelId: string | null;
  disabled: boolean;
  sessionId: string | null;
};

export const ModelPicker = ({ models, currentModelId, disabled, sessionId }: ModelPickerProps) => {
  const current = models.find((m) => m.id === currentModelId);
  const providers = [...new Set(models.map((m) => m.provider))];

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          className="h-7 w-full justify-between gap-1 px-2 font-normal text-xs"
          disabled={disabled}
          size="sm"
          variant="ghost"
        >
          <span className="truncate">{current?.name ?? 'Select model'}</span>
          <ChevronsUpDown className="size-3 shrink-0 text-muted-foreground" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align="start"
        className="max-h-[320px] w-[var(--radix-dropdown-menu-trigger-width)] overflow-y-auto"
      >
        {providers.map((provider, index) => (
          <div key={provider}>
            {index > 0 && <DropdownMenuSeparator />}
            <DropdownMenuGroup>
              <DropdownMenuLabel>{provider}</DropdownMenuLabel>
              {models
                .filter((m) => m.provider === provider)
                .map((m) => (
                  <DropdownMenuItem
                    key={m.id}
                    onSelect={() => sessionId && new ThreadApi(sessionId).setModel(m.id)}
                  >
                    <span className="flex-1 truncate">{m.name}</span>
                    {m.id === currentModelId && <Check className="size-4 shrink-0" />}
                  </DropdownMenuItem>
                ))}
            </DropdownMenuGroup>
          </div>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
};
