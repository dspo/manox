// Wire-api → presentation mapping shared by the model picker and the info
// card's usage rows; labels and variants mirror the host's Tag rendering.

const API_META: Record<string, { label: string; text: string; border: string }> = {
  anthropic: { label: 'Anthropic', text: 'text-primary', border: 'border-primary/50' },
  openai_responses: { label: 'Responses', text: 'text-info', border: 'border-info/50' },
  openai_completions: {
    label: 'Completions',
    text: 'text-warning',
    border: 'border-warning/50',
  },
};

const FALLBACK = {
  label: 'N/A',
  text: 'text-muted-foreground',
  border: 'border-muted-foreground/50',
};

/** Text tint for a bare model-name span (picker trigger, usage rows). */
export const apiTint = (api: string): string => (API_META[api] ?? FALLBACK).text;

/** Outline-badge meta for a model row's wire-api tag. */
export const apiTag = (api: string): { label: string; className: string } => {
  const meta = API_META[api] ?? FALLBACK;
  return { label: meta.label, className: `${meta.border} ${meta.text}` };
};
