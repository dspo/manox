// Composer: textarea with pasted-image chips, a slash-command typeahead,
// and a bottom row carrying the approval-mode toggle and model picker.

import { ArrowUp, Square, X } from 'lucide-react';
import type { ClipboardEvent, KeyboardEvent } from 'react';
import { useCallback, useEffect, useRef, useState } from 'react';

import type { ApprovalMode, CommandEntry, ModelInfo } from '../../../../protocol';
import { ThreadApi } from '../../api/client';
import { store } from '../../state/bridge';
import { cn } from '../../lib/utils';
import { ModelPicker } from './model-picker';
import { Button } from '../ui/button';
import { Textarea } from '../ui/textarea';

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

const ApprovalToggle = ({
  mode,
  disabled,
  onChange,
}: {
  mode: ApprovalMode;
  disabled: boolean;
  onChange: (mode: ApprovalMode) => void;
}) => (
  <div className="border-muted-foreground/20 flex overflow-hidden rounded-md border text-xs">
    {(['autopilot', 'danger'] as const).map((m) => (
      <button
        className={cn(
          'px-2 py-1 capitalize transition-colors',
          mode === m ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-muted',
        )}
        disabled={disabled}
        key={m}
        onClick={() => onChange(m)}
        type="button"
      >
        {m}
      </button>
    ))}
  </div>
);

export type ComposerProps = {
  /** Owning thread; null until a session is established — input is
   * collected but sending is blocked until the host can deliver it. */
  sessionId: string | null;
  turnActive: boolean;
  models: ModelInfo[];
  currentModelId: string | null;
  approvalMode: ApprovalMode;
  commands: CommandEntry[];
};

export const Composer = ({
  sessionId,
  turnActive,
  models,
  currentModelId,
  approvalMode,
  commands,
}: ComposerProps) => {
  const [text, setText] = useState('');
  const [images, setImages] = useState<PastedImage[]>([]);
  const [activeMatch, setActiveMatch] = useState(0);
  const [typeaheadDismissed, setTypeaheadDismissed] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const ready = sessionId !== null;

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
    if ((!trimmed && images.length === 0) || turnActive || !sessionId) {
      return;
    }
    const api = new ThreadApi(sessionId);
    store.echoUser(
      sessionId,
      trimmed,
      images.map((img) => ({ mimeType: img.mimeType, data: img.dataUrl, byteLen: null })),
    );
    api.submit(
      trimmed,
      images.length ? images.map((img) => ({ data: img.data, mimeType: img.mimeType })) : undefined,
    );
    setText('');
    setImages([]);
  }, [text, images, turnActive, sessionId]);

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

  return (
    <div className="border-t p-2">
      {images.length > 0 && (
        <div className="mb-2 flex flex-wrap gap-2">
          {images.map((img, index) => (
            <div className="bg-muted relative rounded-md border" key={index}>
              <img
                alt={`attachment ${index + 1}`}
                className="h-14 w-14 rounded-md object-cover"
                src={img.dataUrl}
              />
              <button
                className="bg-background/90 absolute -top-1.5 -right-1.5 rounded-full border p-0.5"
                onClick={() => setImages((prev) => prev.filter((_, i) => i !== index))}
                title="Remove attachment"
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
          <div className="bg-popover absolute right-0 bottom-full left-0 z-10 mb-1 max-h-48 overflow-y-auto rounded-md border shadow-md">
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
        <div className="flex items-end gap-2">
          <Textarea
            className="min-h-[52px] flex-1 resize-none font-transcript"
            onChange={(e) => setText(e.target.value)}
            onKeyDown={handleKeyDown}
            onPaste={handlePaste}
            placeholder={ready ? 'Message manox' : 'Starting session…'}
            ref={textareaRef}
            rows={2}
            value={text}
          />
          {turnActive && sessionId ? (
            <Button
              onClick={() => new ThreadApi(sessionId).cancel()}
              size="icon"
              title="Stop"
              variant="secondary"
            >
              <Square className="size-4" />
            </Button>
          ) : (
            <Button
              disabled={!ready || (!text.trim() && images.length === 0)}
              onClick={submit}
              size="icon"
              title="Send"
            >
              <ArrowUp className="size-4" />
            </Button>
          )}
        </div>
      </div>
      <div className="mt-1.5 flex items-center gap-2">
        <ApprovalToggle
          disabled={!ready}
          mode={approvalMode}
          onChange={(m) => sessionId && new ThreadApi(sessionId).setApprovalMode(m)}
        />
        <div className="min-w-0 flex-1">
          <ModelPicker
            currentModelId={currentModelId}
            disabled={!ready || turnActive || models.length === 0}
            models={models}
            sessionId={sessionId}
          />
        </div>
      </div>
    </div>
  );
};
