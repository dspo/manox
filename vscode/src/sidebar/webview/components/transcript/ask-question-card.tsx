// AskUserQuestion card: the interactive question drawer rendered from the
// tool-call authorization payload. One question renders per step (the tool
// contract: "Each becomes one step in the question drawer"), mirroring the
// gpui host's drawer. Selections ride back as (question, joined labels)
// answers — the same shape the gpui host submits — and a free-form
// supplemental note rides alongside them, never overriding them.

import { useState } from 'react';
import { Check, ChevronLeft, ChevronRight, X } from 'lucide-react';

import type { AskQuestionWire } from '../../../../protocol';
import { ThreadApi } from '../../api/client';
import { store } from '../../state/bridge';
import { t } from '../../lib/i18n';
import type { TranscriptItem } from '../../state/store';
import { cn } from '../../lib/utils';
import {
  Confirmation,
  ConfirmationAction,
  ConfirmationActions,
  ConfirmationRequest,
  ConfirmationTitle,
} from '../ai/confirmation';
import { Button } from '../ui/button';

export type AskQuestionItem = Extract<TranscriptItem, { kind: 'ask_question' }>;

/** Port of the gpui host's `strip_recommended_suffix`: an option label may
 * carry a "(recommended)" / "（推荐）" suffix in lieu of the explicit flag. */
function stripRecommendedSuffix(label: string): { label: string; recommended: boolean } {
  const lower = label.toLowerCase();
  for (const suffix of [' (Recommended)', '（推荐）', ' (推荐)', '（Recommended）']) {
    if (lower.endsWith(suffix.toLowerCase())) {
      return { label: label.slice(0, label.length - suffix.length).trim(), recommended: true };
    }
  }
  return { label, recommended: false };
}

/** Defensive parse: the tool validates its input before the gate, so a
 * malformed payload here means a protocol regression — fall back to a
 * read-only card instead of crashing the transcript. */
function parseAsk(input: unknown): AskQuestionWire[] | null {
  if (typeof input !== 'object' || input === null) return null;
  const questions = (input as { questions?: unknown }).questions;
  if (!Array.isArray(questions) || !(questions.length >= 1 && questions.length <= 3)) {
    return null;
  }
  const out: AskQuestionWire[] = [];
  for (const q of questions) {
    if (typeof q !== 'object' || q === null) return null;
    const obj = q as Record<string, unknown>;
    const options = obj.options;
    if (!Array.isArray(options) || !(options.length >= 2 && options.length <= 3)) return null;
    out.push({
      question: typeof obj.question === 'string' ? obj.question : '',
      header: typeof obj.header === 'string' ? obj.header : undefined,
      multiSelect: obj.multiSelect === true,
      options: options
        .filter((o): o is Record<string, unknown> => typeof o === 'object' && o !== null)
        .map((o) => {
          const explicitRecommended = o.recommended === true;
          const { label, recommended } = stripRecommendedSuffix(
            typeof o.label === 'string' ? o.label : '',
          );
          return {
            label,
            description: typeof o.description === 'string' ? o.description : undefined,
            recommended: explicitRecommended || recommended,
          };
        }),
    });
  }
  return out;
}

export type AskQuestionCardProps = {
  item: AskQuestionItem;
  sessionId: string;
};

export const AskQuestionCard = ({ item, sessionId }: AskQuestionCardProps) => {
  const questions = parseAsk(item.input);
  // Per-question selected labels; single-select questions replace, multi
  // toggles.
  const [selections, setSelections] = useState<string[][]>(
    () => questions?.map(() => []) ?? [],
  );
  const [step, setStep] = useState(0);
  const [note, setNote] = useState('');

  // Answered state: the drawer is gone; the card shows the actor's rendered
  // verdict (per-question Question/Answer blocks plus any supplemental note)
  // once `tool_result` lands, keeping the human title throughout. A denied
  // or cancelled question renders its output as an error, mirroring the
  // tool card's error styling.
  if (item.answered) {
    return (
      <div className="border-border bg-card text-foreground rounded-lg border px-3 py-2 text-sm">
        <div className="mb-1 font-medium">{item.summary}</div>
        {item.output && (
          <pre
            className={cn(
              'whitespace-pre-wrap font-code text-xs',
              item.isError ? 'text-danger' : 'text-muted-foreground',
            )}
          >
            {item.output}
          </pre>
        )}
      </div>
    );
  }

  if (!questions) {
    // Protocol-regression fallback: the payload the tool already validated
    // is unreadable here. Render the summary + raw input read-only — an
    // Approve/Deny pair would mislabel the outcome (every bare decision on
    // the actor surfaces as a denial).
    return (
      <div className="border-border bg-card text-foreground rounded-lg border px-3 py-2 text-sm">
        <div className="mb-1">{item.summary}</div>
        <pre className="text-muted-foreground font-code max-h-[180px] overflow-auto rounded-md bg-muted/50 p-2 text-xs">
          {JSON.stringify(item.input, null, 2)}
        </pre>
      </div>
    );
  }

  const total = questions.length;
  const current = questions[Math.min(step, total - 1)];
  const isLast = step === total - 1;
  const canPrev = step > 0;

  const toggle = (label: string, multi: boolean) => {
    setSelections((prev) =>
      prev.map((sel, i) =>
        i !== step
          ? sel
          : multi
            ? sel.includes(label)
              ? sel.filter((l) => l !== label)
              : [...sel, label]
            : [label],
      ),
    );
  };

  const submit = () => {
    const answers: [string, string][] = questions.map((q, i) => [
      q.question,
      (selections[i] ?? []).join(', '),
    ]);
    const trimmed = note.trim();
    new ThreadApi(sessionId).answerQuestion(
      item.id,
      answers,
      trimmed.length > 0 ? trimmed : null,
    );
    store.respondAsk(sessionId, item.id);
  };

  const cancel = () => {
    new ThreadApi(sessionId).approve(item.id, false);
    store.respondAsk(sessionId, item.id);
  };

  const next = () => setStep((s) => Math.min(total - 1, s + 1));
  const prev = () => setStep((s) => Math.max(0, s - 1));

  // The drawer title follows the gpui host: the step's header when present,
  // otherwise the generic clarification title carried in the summary.
  const title = current.header?.trim() ? current.header : item.summary;

  return (
    <Confirmation approval={{ id: item.id, approved: false }} state="approval-requested" variant="default">
      <ConfirmationTitle>
        <div className="flex w-full items-center justify-between gap-2">
          <span className="text-foreground min-w-0 flex-1 truncate text-sm font-medium">
            {title}
          </span>
          <nav className="flex shrink-0 items-center gap-0.5">
            <Button
              aria-label={t('ask_prev_question')}
              disabled={!canPrev}
              onClick={prev}
              size="icon-sm"
              variant="ghost"
            >
              <ChevronLeft />
            </Button>
            <span className="text-muted-foreground min-w-[44px] text-center text-xs">
              {step + 1} of {total}
            </span>
            <Button
              aria-label={isLast ? t('ask_submit') : t('ask_next_question')}
              onClick={isLast ? submit : next}
              size="icon-sm"
              variant="ghost"
            >
              {isLast ? <Check /> : <ChevronRight />}
            </Button>
            <Button aria-label={t('ask_cancel')} onClick={cancel} size="icon-sm" variant="ghost">
              <X />
            </Button>
          </nav>
        </div>
      </ConfirmationTitle>
      <ConfirmationRequest>
        <div className="space-y-3">
          <p className="text-sm">{current.question}</p>
          <div className="space-y-1">
            {current.options.map((o) => {
              const selected = (selections[step] ?? []).includes(o.label);
              return (
                <button
                  className={cn(
                    'block w-full cursor-pointer rounded-md border border-border px-2 py-1.5 text-left text-sm transition-colors hover:bg-accent',
                    selected && 'border-info text-info bg-accent',
                  )}
                  key={o.label}
                  onClick={() => toggle(o.label, current.multiSelect === true)}
                  type="button"
                >
                  <span className="font-medium">
                    {current.multiSelect ? (selected ? '☑ ' : '☐ ') : selected ? '● ' : '○ '}
                    {o.label}
                    {o.recommended ? ` (${t('ask_recommended')})` : ''}
                  </span>
                  {o.description && (
                    <span className="text-muted-foreground block text-xs">{o.description}</span>
                  )}
                </button>
              );
            })}
          </div>
          <input
            className="border-border bg-background text-foreground w-full rounded-md border px-2 py-1 text-sm"
            onChange={(e) => setNote(e.target.value)}
            placeholder={t('ask_note_placeholder')}
            value={note}
          />
        </div>
      </ConfirmationRequest>
      <ConfirmationActions>
        <ConfirmationAction onClick={cancel} variant="outline">
          {t('ask_cancel')}
        </ConfirmationAction>
        <ConfirmationAction onClick={submit}>{t('ask_submit')}</ConfirmationAction>
      </ConfirmationActions>
    </Confirmation>
  );
};
