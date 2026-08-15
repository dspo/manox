// Composer: borderless input under a 1px hairline, pasted-image chips, a
// slash-command typeahead, and a bottom row carrying the approval-mode
// dropdown on the left and the model picker plus send button on the right.

import { ArrowUp, Bot, Check, ChevronDown, Pause, TriangleAlert, X } from 'lucide-react';
import type { ClipboardEvent, KeyboardEvent } from 'react';
import { useCallback, useEffect, useRef, useState } from 'react';

import type { ApprovalMode, CommandEntry, ModelInfo } from '../../../../protocol';
import { ThreadApi } from '../../api/client';
import { t } from '../../lib/i18n';
import { cn } from '../../lib/utils';
import { store } from '../../state/bridge';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '../ui/dropdown-menu';
import { Textarea } from '../ui/textarea';
import { ModelPicker } from './model-picker';

const MAX_IMAGE_EDGE_PX = 1568;
const MAX_IMAGE_BYTES = 5 * 1024 * 1024;
const IMAGE_MIMES = new Set(['image/png', 'image/jpeg', 'image/webp', 'image/gif']);

/** Chip state while attached; `preview` is a renderable data url, `data`
 * the bare base64 payload that goes on the wire. */
interface PastedImage {
  data: string;
  dataUrl: string;
  mimeType: string;
}

// Downscale to the model's preferred edge and re-encode as PNG; oversized
// results are rejected rather than sent.
async function fileToImage(file: File): Promise<PastedImage | null> {
  const dataUrl = await new Promise<string>((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as string);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
  const img = new Image();
  await new Promise<void>((resolve, reject) => {
    img.onload = () => resolve();
    img.onerror = () => reject(new Error('undecodable image'));
    img.src = dataUrl;
  });
  const scale = Math.min(1, MAX_IMAGE_EDGE_PX / Math.max(img.width, img.height));
  const canvas = document.createElement('canvas');
  canvas.width = Math.max(1, Math.round(img.width * scale));
  canvas.height = Math.max(1, Math.round(img.height * scale));
  const context = canvas.getContext('2d');
  if (!context) return null;
  context.drawImage(img, 0, 0, canvas.width, canvas.height);
  const encoded = canvas.toDataURL('image/png');
  const base64 = encoded.slice(encoded.indexOf(',') + 1);
  if (Math.ceil((base64.length * 3) / 4) > MAX_IMAGE_BYTES) return null;
  return { data: base64, dataUrl: encoded, mimeType: 'image/png' };
}

const APPROVAL_META = {
  autopilot: { icon: Bot, tint: 'text-info', labelKey: 'autopilot', descKey: 'autopilot_desc' },
  danger: {
    icon: TriangleAlert,
    tint: 'text-danger',
    labelKey: 'danger',
    descKey: 'danger_desc',
  },
} as const;

const ApprovalChip = ({
  mode,
  disabled,
  onChange,
}: {
  mode: ApprovalMode;
  disabled: boolean;
  onChange: (mode: ApprovalMode) => void;
}) => {
  const { icon: Icon, tint, labelKey } = APPROVAL_META[mode];
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          className={cn(
            'hover:bg-accent flex min-w-24 cursor-pointer items-center gap-1.5 rounded-full border border-border px-2 py-1 text-xs transition-colors',
            disabled && 'pointer-events-none opacity-50',
          )}
          disabled={disabled}
          type="button"
        >
          <Icon className={cn('size-3.5', tint)} />
          <span>{t(labelKey)}</span>
          <ChevronDown className="text-muted-foreground size-3" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-[360px]">
        <div className="px-2 py-1.5 text-sm font-medium">{t('approval_mode')}</div>
        <DropdownMenuSeparator />
        {(['autopilot', 'danger'] as const).map((m) => {
          const option = APPROVAL_META[m];
          const OptionIcon = option.icon;
          return (
            <DropdownMenuItem className="gap-2.5" key={m} onSelect={() => onChange(m)}>
              <OptionIcon className={cn('size-4 shrink-0', option.tint)} />
              <div className="min-w-0 flex-1">
                <p className={cn('text-xs font-medium', option.tint)}>{t(option.labelKey)}</p>
                <p className="text-muted-foreground text-xs">{t(option.descKey)}</p>
              </div>
              {mode === m && <Check className="size-4 shrink-0" />}
            </DropdownMenuItem>
          );
        })}
      </DropdownMenuContent>
    </DropdownMenu>
  );
};

export type ComposerProps = {
  /** Owning thread; null until a session is established. With an
   * `onCreateSession` callback the composer works as a draft input whose
   * first send creates the thread; without it, input is collected but
   * sending is blocked until the host can deliver it. */
  sessionId: string | null;
  turnActive: boolean;
  models: ModelInfo[];
  currentModelId: string | null;
  approvalMode: ApprovalMode;
  commands: CommandEntry[];
  /** Drafted session still waiting for the host's confirmation. */
  creating?: boolean;
  /** Draft-mode send: creates the session and delivers the first message. */
  onCreateSession?: (text: string, images: { data: string; mimeType: string }[]) => void;
  /** Draft-mode model selection; the chosen id rides along on creation. */
  onModelChange?: (modelId: string) => void;
};

export const Composer = ({
  sessionId,
  turnActive,
  models,
  currentModelId,
  approvalMode,
  commands,
  creating = false,
  onCreateSession,
  onModelChange,
}: ComposerProps) => {
  const [text, setText] = useState('');
  const [images, setImages] = useState<PastedImage[]>([]);
  const [activeMatch, setActiveMatch] = useState(0);
  const [typeaheadDismissed, setTypeaheadDismissed] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const draft = sessionId === null && onCreateSession !== undefined;
  const ready = sessionId !== null || draft;

  // The typeahead is live only while the leading token is an unfinished
  // slash invocation; the actor does the actual routing on submit.
  const slashPrefix = /^\/(\S*)$/.exec(text)?.[1]?.toLowerCase();
  const matches =
    slashPrefix === undefined || typeaheadDismissed
      ? []
      : commands.filter((c) => c.name.toLowerCase().startsWith(slashPrefix));
  const showTypeahead = matches.length > 0;

  useEffect(() => {
    setActiveMatch(0);
    setTypeaheadDismissed(false);
  }, [text]);

  const complete = useCallback((entry: CommandEntry) => {
    setText(`/${entry.name} `);
    setTypeaheadDismissed(true);
    textareaRef.current?.focus();
  }, []);

  const submit = useCallback(() => {
    const trimmed = text.trim();
    if ((!trimmed && images.length === 0) || turnActive || creating) {
      return;
    }
    const wireImages = images.length
      ? images.map((img) => ({ data: img.data, mimeType: img.mimeType }))
      : undefined;
    if (sessionId) {
      const api = new ThreadApi(sessionId);
      store.echoUser(
        sessionId,
        trimmed,
        images.map((img) => ({ mimeType: img.mimeType, data: img.dataUrl, byteLen: null })),
      );
      api.submit(trimmed, wireImages);
    } else if (onCreateSession) {
      onCreateSession(trimmed, wireImages ?? []);
    } else {
      return;
    }
    setText('');
    setImages([]);
  }, [text, images, turnActive, creating, sessionId, onCreateSession]);

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (showTypeahead) {
      if (e.key === 'Tab') {
        e.preventDefault();
        complete(matches[Math.min(activeMatch, matches.length - 1)]);
        return;
      }
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        setActiveMatch((i) => (i + 1) % matches.length);
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        setActiveMatch((i) => (i - 1 + matches.length) % matches.length);
        return;
      }
      if (e.key === 'Escape') {
        e.preventDefault();
        setTypeaheadDismissed(true);
        return;
      }
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  };

  const handlePaste = async (e: ClipboardEvent<HTMLTextAreaElement>) => {
    const files = Array.from(e.clipboardData.files).filter((f) => IMAGE_MIMES.has(f.type));
    if (files.length === 0) return;
    e.preventDefault();
    const pasted = (await Promise.all(files.map(fileToImage))).filter(
      (img): img is PastedImage => img !== null,
    );
    if (pasted.length > 0) setImages((prev) => [...prev, ...pasted]);
  };

  const canSend = ready && !creating && (text.trim() !== '' || images.length > 0);

  return (
    <div className="border-t border-border">
      {images.length > 0 && (
        <div className="flex flex-wrap gap-2 px-3 pt-2">
          {images.map((img, index) => (
            <div className="bg-muted relative rounded-md border" key={index}>
              <img
                alt={`attachment ${index + 1}`}
                className="h-14 w-14 rounded-md object-cover"
                src={img.dataUrl}
              />
              <button
                className="bg-background/90 absolute -top-1.5 -right-1.5 cursor-pointer rounded-full border p-0.5"
                onClick={() => setImages((prev) => prev.filter((_, i) => i !== index))}
                title={t('remove_attachment')}
                type="button"
              >
                <X className="size-3" />
              </button>
            </div>
          ))}
        </div>
      )}
      <div className="relative">
        {showTypeahead && (
          <div className="bg-card absolute right-2 bottom-full left-2 z-10 mb-1 max-h-48 overflow-y-auto rounded-md border shadow-md">
            {matches.map((entry, index) => (
              <button
                className={cn(
                  'flex w-full items-center gap-2 px-2 py-1.5 text-left text-xs',
                  index === activeMatch && 'bg-muted',
                )}
                key={entry.name}
                onMouseDown={(e) => {
                  e.preventDefault();
                  complete(entry);
                }}
                type="button"
              >
                <span className="shrink-0 font-medium">/{entry.name}</span>
                <span className="border-muted-foreground/30 text-muted-foreground shrink-0 rounded border px-1 text-[10px]">
                  {entry.kind}
                </span>
                <span className="text-muted-foreground min-w-0 flex-1 truncate">
                  {entry.description}
                </span>
              </button>
            ))}
          </div>
        )}
        <Textarea
          className="font-code min-h-[52px] resize-none border-0 bg-transparent px-3 py-2 text-[13px] font-light shadow-none focus-visible:ring-0"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={handleKeyDown}
          onPaste={handlePaste}
          placeholder={ready ? t('composer_placeholder') : t('starting_session')}
          ref={textareaRef}
          rows={2}
          value={text}
        />
      </div>
      <div
        className={cn(
          'flex items-center px-3 pb-2',
          draft ? 'justify-end' : 'justify-between',
        )}
      >
        {/* Approval policy needs a live session; the draft's first send
         * creates one with the host's defaults. Model selection works for
         * drafts too: the choice rides along on creation. */}
        {!draft && (
          <ApprovalChip
            disabled={!ready}
            mode={approvalMode}
            onChange={(m) => sessionId && new ThreadApi(sessionId).setApprovalMode(m)}
          />
        )}
        <div className="flex items-center gap-2">
          <ModelPicker
            currentModelId={currentModelId}
            disabled={!ready || creating}
            models={models}
            onSelect={draft ? onModelChange : undefined}
            sessionId={sessionId}
          />
          {turnActive && sessionId ? (
            <button
              className="bg-danger/20 text-danger hover:bg-danger/30 flex size-6 shrink-0 cursor-pointer items-center justify-center rounded-full transition-colors"
              onClick={() => new ThreadApi(sessionId).cancel()}
              title={t('stop')}
              type="button"
            >
              <Pause className="size-3.5" />
            </button>
          ) : (
            <button
              className={cn(
                'bg-primary/20 text-primary hover:bg-primary/30 flex size-6 shrink-0 items-center justify-center rounded-full transition-colors',
                canSend ? 'cursor-pointer' : 'pointer-events-none opacity-40',
              )}
              disabled={!canSend}
              onClick={submit}
              title={t('send')}
              type="button"
            >
              <ArrowUp className="size-3.5" />
            </button>
          )}
        </div>
      </div>
    </div>
  );
};
