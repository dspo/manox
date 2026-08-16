// AskUserQuestion card: the interactive question drawer rendered from the
// tool-call authorization payload. Selections ride back as (question,
// joined labels) answers — the same shape the gpui host submits — and a
// free-form note dismisses the whole card, overriding selections.

import { useState } from 'react';

import type { AskQuestionWire } from '../../../../protocol';
import { ThreadApi } from '../../api/client';
import { store } from '../../state/bridge';
import { t } from '../../lib/i18n';
import type { TranscriptItem } from '../../state/store';
import { cn } from '../../lib/utils';
import { ApprovalCard } from './approval-card';
import {
  Confirmation,
  ConfirmationAction,
  ConfirmationActions,
  ConfirmationRequest,
  ConfirmationTitle,
} from '../ai/confirmation';

export type AskQuestionItem = Extract<TranscriptItem, { kind: 'ask_question' }>;

/** Defensive parse: the tool validates its input before the gate, so a
 * malformed payload here means a protocol regression — fall back to the
 * generic approval card instead of crashing the transcript. */
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
        .map((o) => ({
          label: typeof o.label === 'string' ? o.label : '',
          description: typeof o.description === 'string' ? o.description : undefined,
          recommended: o.recommended === true,
        })),
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
  const [note, setNote] = useState('');

  if (!questions) {
    return (
      <ApprovalCard
        item={{ id: item.id, input: item.input, kind: 'approval', summary: item.summary, toolName: 'AskUserQuestion' }}
        sessionId={sessionId}
      />
    );
  }

  const toggle = (qi: number, label: string, multi: boolean) => {
    setSelections((prev) =>
      prev.map((sel, i) =>
        i !== qi
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
    store.decideApproval(sessionId, item.id);
  };

  const cancel = () => {
    new ThreadApi(sessionId).approve(item.id, false);
    store.decideApproval(sessionId, item.id);
  };

  return (
    <Confirmation approval={{ id: item.id, approved: false }} state="approval-requested" variant="default">
      <ConfirmationTitle>{item.summary}</ConfirmationTitle>
      <ConfirmationRequest>
        <div className="space-y-3">
          {questions.map((q, qi) => (
            <div key={qi}>
              {q.header && (
                <p className="text-muted-foreground mb-0.5 text-xs uppercase">{q.header}</p>
              )}
              <p className="mb-1 text-sm">{q.question}</p>
              <div className="space-y-1">
                {q.options.map((o) => {
                  const selected = (selections[qi] ?? []).includes(o.label);
                  return (
                    <button
                      className={cn(
                        'block w-full cursor-pointer rounded-md border border-border px-2 py-1.5 text-left text-sm transition-colors hover:bg-accent',
                        selected && 'border-info text-info bg-accent',
                      )}
                      key={o.label}
                      onClick={() => toggle(qi, o.label, q.multiSelect === true)}
                      type="button"
                    >
                      <span className="font-medium">
                        {q.multiSelect ? (selected ? '☑ ' : '☐ ') : selected ? '● ' : '○ '}
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
            </div>
          ))}
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
