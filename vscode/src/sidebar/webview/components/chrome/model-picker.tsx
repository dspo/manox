import { Check, ChevronUp } from 'lucide-react';

import type { ModelInfo } from '../../../../protocol';
import { ThreadApi } from '../../api/client';
import { t } from '../../lib/i18n';
import { cn } from '../../lib/utils';
import { Badge } from '../ui/badge';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from '../ui/dropdown-menu';

// Model names take the tint of their wire api, mirroring the host's picker.
const API_TINTS: Record<string, string> = {
  anthropic: 'text-primary',
  openai_responses: 'text-info',
  openai_completions: 'text-warning',
};

const apiTint = (api: string): string => API_TINTS[api] ?? 'text-foreground';

export type ModelPickerProps = {
  models: ModelInfo[];
  currentModelId: string | null;
  disabled: boolean;
  sessionId: string | null;
  /** Selection without a live session (a draft thread's first send applies
   * the model at creation time). Takes precedence over the sessionId path. */
  onSelect?: (modelId: string) => void;
};

export const ModelPicker = ({
  models,
  currentModelId,
  disabled,
  sessionId,
  onSelect,
}: ModelPickerProps) => {
  const current = models.find((m) => m.id === currentModelId);
  const providers = [...new Set(models.map((m) => m.provider))];
  const select = (modelId: string) => {
    if (onSelect) {
      onSelect(modelId);
    } else if (sessionId) {
      new ThreadApi(sessionId).setModel(modelId);
    }
  };

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          className={cn(
            'flex cursor-pointer items-center gap-1 text-xs',
            disabled && 'pointer-events-none opacity-50',
          )}
          disabled={disabled}
          type="button"
        >
          {current ? (
            <>
              <span className="shrink-0">{current.provider}</span>
              <span className="text-muted-foreground shrink-0">·</span>
              <span className={cn('max-w-40 truncate', apiTint(current.api))}>
                {current.name}
              </span>
            </>
          ) : (
            <span className="text-muted-foreground">{t('select_model')}</span>
          )}
          <ChevronUp className="text-muted-foreground size-3 shrink-0" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="max-h-[320px] w-52 overflow-y-auto">
        {providers.map((provider) => (
          <DropdownMenuSub key={provider}>
            <DropdownMenuSubTrigger>
              <span className="truncate text-sm font-medium">{provider}</span>
            </DropdownMenuSubTrigger>
            <DropdownMenuSubContent className="max-h-[320px] w-64 overflow-y-auto">
              {models
                .filter((m) => m.provider === provider)
                .map((m) => (
                  <DropdownMenuItem
                    className="gap-2"
                    key={m.id}
                    onSelect={() => select(m.id)}
                  >
                    <Badge className="px-1 text-[10px] font-normal" variant="outline">
                      {m.api}
                    </Badge>
                    <span className="flex-1 truncate">{m.name}</span>
                    {m.id === currentModelId && <Check className="size-4 shrink-0" />}
                  </DropdownMenuItem>
                ))}
            </DropdownMenuSubContent>
          </DropdownMenuSub>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
};
