import { Check, ChevronDown, ChevronUp } from 'lucide-react';
import { useState } from 'react';

import type { ModelInfo, ReasoningEffort } from '../../../../protocol';
import { ThreadApi } from '../../api/client';
import { findCurrentModel } from '../../lib/current-model';
import { apiTag, apiTint } from '../../lib/api-tint';
import { t } from '../../lib/i18n';
import { cn } from '../../lib/utils';
import { Badge } from '../ui/badge';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from '../ui/dropdown-menu';

const REASONING_EFFORTS: ReasoningEffort[] = ['high', 'max'];

export type ModelPickerProps = {
  models: ModelInfo[];
  currentModelId: string | null;
  disabled: boolean;
  sessionId: string | null;
  reasoningEffort: ReasoningEffort;
  /** Selection without a live session (a draft thread's first send applies
   * the model at creation time). Takes precedence over the sessionId path. */
  onSelect?: (modelId: string) => void;
};

export const ModelPicker = ({
  models,
  currentModelId,
  disabled,
  sessionId,
  reasoningEffort,
  onSelect,
}: ModelPickerProps) => {
  const [open, setOpen] = useState(false);
  const current = findCurrentModel(models, currentModelId);
  // Group by provider DISPLAY name with same-name registrations merged into
  // one submenu, mirroring the host's popup grouping.
  const groups: { name: string; models: ModelInfo[] }[] = [];
  for (const m of models) {
    const name = m.provider_name ?? m.provider;
    const group = groups.find((g) => g.name === name);
    if (group) {
      group.models.push(m);
    } else {
      groups.push({ name, models: [m] });
    }
  }
  const select = (modelId: string) => {
    if (onSelect) {
      onSelect(modelId);
    } else if (sessionId) {
      new ThreadApi(sessionId).setModel(modelId);
    }
  };

  return (
    <DropdownMenu onOpenChange={setOpen}>
      <DropdownMenuTrigger asChild>
        <button
          className={cn(
            'hover:bg-accent/8 flex cursor-pointer items-center gap-1 rounded px-2 py-1 text-xs',
            disabled && 'pointer-events-none opacity-50',
          )}
          disabled={disabled}
          type="button"
        >
          {current ? (
            <>
              <span className="shrink-0">{current.provider_name ?? current.provider}</span>
              <span className="text-muted-foreground shrink-0">·</span>
              <span className={cn('max-w-40 truncate', apiTint(current.api))}>
                {current.name}
              </span>
            </>
          ) : (
            <span>{t('no_model_configured')}</span>
          )}
          {open ? (
            <ChevronUp className="text-muted-foreground size-3 shrink-0" />
          ) : (
            <ChevronDown className="text-muted-foreground size-3 shrink-0" />
          )}
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align="end"
        className="max-h-[320px] w-52 overflow-y-auto"
        side="top"
      >
        {groups.length === 0 ? (
          <DropdownMenuLabel className="text-muted-foreground font-normal">
            {t('no_models_configured')}
          </DropdownMenuLabel>
        ) : (
          groups.map((group) => (
            <DropdownMenuSub key={group.name}>
              <DropdownMenuSubTrigger>
                <span className="truncate text-sm font-medium">{group.name}</span>
              </DropdownMenuSubTrigger>
              <DropdownMenuSubContent className="max-h-[320px] w-64 overflow-y-auto">
                {group.models.map((m) => {
                  const tag = apiTag(m.api);
                  return (
                    <DropdownMenuItem
                      className="gap-2"
                      key={`${m.provider}/${m.id}`}
                      onSelect={() => select(`${m.provider}/${m.id}`)}
                    >
                      <Badge
                        className={cn('px-1 text-[10px] font-normal', tag.className)}
                        variant="outline"
                      >
                        {tag.label}
                      </Badge>
                      <span className="flex-1 truncate">{m.name}</span>
                    </DropdownMenuItem>
                  );
                })}
              </DropdownMenuSubContent>
            </DropdownMenuSub>
          ))
        )}
        {/* The effort knob tunes the same model the submenus above pick; the
         * current effort is checked and a click applies to the next request.
         * Draft threads have no session yet, so the knob stays disabled. */}
        <DropdownMenuSeparator />
        <DropdownMenuLabel className="text-muted-foreground font-normal">
          {t('reasoning_effort')}
        </DropdownMenuLabel>
        {REASONING_EFFORTS.map((effort) => (
          <DropdownMenuItem
            disabled={!sessionId || disabled}
            key={effort}
            onSelect={() => sessionId && new ThreadApi(sessionId).setReasoningEffort(effort)}
          >
            <span className="flex-1">{t(effort === 'high' ? 'reasoning_high' : 'reasoning_max')}</span>
            {reasoningEffort === effort && <Check className="size-3.5" />}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
};
