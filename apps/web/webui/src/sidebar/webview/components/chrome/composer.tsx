// Composer: borderless input under a 1px hairline, pasted-image chips, a
// slash-command typeahead, and a bottom row carrying the approval-mode
// dropdown on the left and the model picker plus send button on the right.

import { ArrowUp, Check, ChevronDown, Lock, Pause, ShieldCheck, TriangleAlert, X } from 'lucide-react';
import type { ClipboardEvent, KeyboardEvent } from 'react';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';

import type { ApprovalMode, CommandEntry, ModelInfo, ReasoningEffort } from '../../../../protocol';
import { ThreadApi } from '../../api/client';
import { hasCommandKey, t, type I18nKey } from '../../lib/i18n';
import { enterAction } from '../../lib/ime';
import { recallStep } from '../../lib/turn-recall';
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
// Trailing-key window after `compositionend`: only an Enter landing this
// fast is the engine's post-composition echo, never a deliberate send.
const COMPOSITION_TRAIL_WINDOW_MS = 300;
const MAX_IMAGE_BYTES = 5 * 1024 * 1024;
const IMAGE_MIMES = new Set(['image/png', 'image/jpeg', 'image/webp', 'image/gif']);
// Navigation built-ins (`/exit` / `/new` and their aliases) take effect on
// submit even while a turn is running: the actor cancels the in-flight turn
// and disposes the session.
const NAV_BUILTIN = /^\s*\/(?:exit|quit|new|clear|archive)(?:\s|$)/;

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
  'read-only': {
    icon: Lock,
    tint: 'text-warning',
    labelKey: 'read_only',
    descKey: 'read_only_desc',
  },
  'workspace-write': {
    icon: ShieldCheck,
    tint: 'text-info',
    labelKey: 'workspace_write',
    descKey: 'workspace_write_desc',
  },
  'danger-full-access': {
    icon: TriangleAlert,
    tint: 'text-danger',
    labelKey: 'danger_full_access',
    descKey: 'danger_full_access_desc',
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
        {(['read-only', 'workspace-write', 'danger-full-access'] as const).map((m) => {
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

/** Plan-mode toggle chip: a one-click switch next to the approval chip. */
const PlanChip = ({
  enabled,
  disabled,
  onChange,
}: {
  enabled: boolean;
  disabled: boolean;
  onChange: (enabled: boolean) => void;
}) => (
  <button
    className={cn(
      'flex min-w-24 cursor-pointer items-center gap-1.5 rounded-full border border-border px-2 py-1 text-xs transition-colors',
      enabled ? 'text-info hover:bg-accent' : 'hover:bg-accent text-muted-foreground',
      disabled && 'pointer-events-none opacity-50',
    )}
    disabled={disabled}
    onClick={() => onChange(!enabled)}
    title={t('plan_mode')}
    type="button"
  >
    <span>{t('plan')}</span>
    <span className={cn(enabled ? 'text-info' : 'text-muted-foreground')}>
      {t(enabled ? 'plan_mode_on' : 'plan_mode_off')}
    </span>
  </button>
);

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
  reasoningEffort: ReasoningEffort;
  planMode: boolean;
  commands: CommandEntry[];
  /** Drafted session still waiting for the host's confirmation. */
  creating?: boolean;
  /** Draft-mode send: creates the session and delivers the first message. */
  onCreateSession?: (text: string, images: { data: string; mimeType: string }[]) => void;
  /** Draft-mode model selection; the chosen id rides along on creation. */
  onModelChange?: (modelId: string) => void;
  /** Newest-first user-turn texts of the owning thread, for ↑ recall. */
  userTurns?: { id: string; text: string }[];
  /** Opens the turn navigator (cmd/ctrl+m while the input is focused). */
  onOpenTurnNavigator?: () => void;
  /** Parent-owned ref for refocusing the composer when the navigator closes. */
  composerInputRef?: React.RefObject<HTMLTextAreaElement | null>;
};

export const Composer = ({
  sessionId,
  turnActive,
  models,
  currentModelId,
  approvalMode,
  reasoningEffort,
  planMode,
  commands,
  creating = false,
  userTurns = [],
  onOpenTurnNavigator,
  composerInputRef,
  onCreateSession,
  onModelChange,
}: ComposerProps) => {
  const [text, setText] = useState('');
  const [images, setImages] = useState<PastedImage[]>([]);
  const [activeMatch, setActiveMatch] = useState(0);
  const [typeaheadDismissed, setTypeaheadDismissed] = useState(false);
  // Recall position into `userTurns` (newest-first); -1 = not recalling.
  // Derived state: the reducer exits recall as soon as the value diverges
  // from the recalled text, so typing/sending resets it implicitly.
  const [recallIndex, setRecallIndex] = useState(-1);
  const fallbackRef = useRef<HTMLTextAreaElement>(null);
  const textareaRef = composerInputRef ?? fallbackRef;
  // IME composition state: while composing, and for the trailing Enter some
  // engines fire right after `compositionend`, the key is deferred to the
  // IME. The timestamp window keeps the deferral tight so a later genuine
  // Enter (e.g. after a click-committed composition) still sends.
  const compositionEndedAtRef = useRef(0);
  const draft = sessionId === null && onCreateSession !== undefined;
  const ready = sessionId !== null || draft;

  // The typeahead is live only while the leading token is an unfinished
  // slash invocation; the actor does the actual routing on submit. Leading
  // whitespace is tolerated, matching the gpui host's parser.
  const slashPrefix = /^\s*\/(\S*)$/.exec(text)?.[1]?.toLowerCase();
  const matches =
    slashPrefix === undefined || typeaheadDismissed
      ? []
      : commands.filter((c) => c.name.toLowerCase().startsWith(slashPrefix));
  const showTypeahead = matches.length > 0;

  useEffect(() => {
    setActiveMatch(0);
    setTypeaheadDismissed(false);
  }, [text]);
  // The recall walk is over the owning thread's user turns, newest first.
  const recallTurns = useMemo(() => userTurns.map((turn) => turn.text), [userTurns]);

  const complete = useCallback((entry: CommandEntry) => {
    setText(`/${entry.name} `);
    setTypeaheadDismissed(true);
    textareaRef.current?.focus();
  }, []);

  /** One-line typeahead description: built-ins translate through the
   * webview's own chrome locale; unknown keys fall back to the raw
   * description the actor shipped. */
  const describe = (entry: CommandEntry): string =>
    entry.i18n_key && hasCommandKey(entry.i18n_key)
      ? t(entry.i18n_key as I18nKey)
      : (entry.description ?? `/${entry.name}`);
  const submit = useCallback(() => {
    const trimmed = text.trim();
    // Navigation built-ins bypass the turn-active guard: the actor cancels
    // any in-flight turn, so `/exit` etc. return to the thread list
    // immediately. Attached images fall through — the actor only routes
    // slashes on text-only submissions.
    if (sessionId && !creating && images.length === 0 && NAV_BUILTIN.test(trimmed)) {
      store.echoUser(sessionId, trimmed);
      store.backToList();
      new ThreadApi(sessionId).submit(trimmed);
      setText('');
      setImages([]);
      return;
    }
    if ((!trimmed && images.length === 0) || creating) {
      return;
    }
    const wireImages = images.length
      ? images.map((img) => ({ data: img.data, mimeType: img.mimeType }))
      : undefined;
    if (sessionId) {
      const api = new ThreadApi(sessionId);
      // A submit while a turn runs parks the message on the actor; the
      // bubble renders queued until the drain or a steer/drop action.
      const clientId = crypto.randomUUID();
      store.echoUser(
        sessionId,
        trimmed,
        images.map((img) => ({ mimeType: img.mimeType, data: img.dataUrl, byteLen: null })),
        { queued: turnActive, clientId },
      );
      api.submit(trimmed, wireImages, clientId);
    } else if (onCreateSession) {
      onCreateSession(trimmed, wireImages ?? []);
    } else {
      return;
    }
    setText('');
    setImages([]);
  }, [text, images, turnActive, creating, sessionId, onCreateSession]);

  const handleCompositionEnd = () => {
    compositionEndedAtRef.current = Date.now();
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    // Cmd/Ctrl-M opens the turn navigator, mirroring the gpui host's
    // binding; the composer only owns the trigger, the parent renders it.
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'm') {
      if (!onOpenTurnNavigator) return;
      if (e.nativeEvent.isComposing) return;
      e.preventDefault();
      onOpenTurnNavigator();
      return;
    }
    if (e.key === 'Enter') {
      const trailingComposition =
        Date.now() - compositionEndedAtRef.current < COMPOSITION_TRAIL_WINDOW_MS;
      compositionEndedAtRef.current = 0;
      const action = enterAction(
        {
          key: e.key,
          shiftKey: e.shiftKey,
          isComposing: e.nativeEvent.isComposing,
          keyCode: e.nativeEvent.keyCode,
        },
        trailingComposition,
      );
      // Deferred keys (IME commit, shift+Enter newline, trailing composition
      // key) keep the browser's default behavior.
      if (action === 'defer') return;
      e.preventDefault();
      submit();
      return;
    }
    // Any other key clears the composition trail and drives the typeahead.
    compositionEndedAtRef.current = 0;
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
    // History recall: the typeahead above already consumed ArrowUp/Down
    // while visible, so reaching here means the arrows belong to the input.
    if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
      if (e.nativeEvent.isComposing) return;
      const { index, step } = recallStep(
        e.key === 'ArrowUp' ? 'up' : 'down',
        text,
        recallIndex,
        recallTurns,
      );
      setRecallIndex(index);
      if (step.kind === 'none') return;
      e.preventDefault();
      if (step.kind === 'clear') {
        setText('');
      } else {
        setText(step.text);
        requestAnimationFrame(() => {
          const el = textareaRef.current;
          if (el) el.setSelectionRange(el.value.length, el.value.length);
        });
      }
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
                  {describe(entry)}
                </span>
              </button>
            ))}
          </div>
        )}
        <Textarea
          className="font-code min-h-[85px] resize-none border-0 bg-transparent px-3 py-2 text-[13px] font-light shadow-none focus-visible:ring-0"
          onChange={(e) => setText(e.target.value)}
          onCompositionEnd={handleCompositionEnd}
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
          <div className="flex items-center gap-1.5">
            <ApprovalChip
              disabled={!ready}
              mode={approvalMode}
              onChange={(m) => sessionId && new ThreadApi(sessionId).setApprovalMode(m)}
            />
            <PlanChip
              disabled={!ready}
              enabled={planMode}
              onChange={(enabled) => sessionId && new ThreadApi(sessionId).setPlanMode(enabled)}
            />
          </div>
        )}
        <div className="flex items-center gap-2">
          <ModelPicker
            currentModelId={currentModelId}
            disabled={!ready || creating}
            models={models}
            onSelect={draft ? onModelChange : undefined}
            reasoningEffort={reasoningEffort}
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
