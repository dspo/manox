// Wire-api → text tint mapping shared by the model picker and the info
// card's usage rows.

const API_TINTS: Record<string, string> = {
  anthropic: 'text-primary',
  openai_responses: 'text-info',
  openai_completions: 'text-warning',
};

export const apiTint = (api: string): string => API_TINTS[api] ?? 'text-foreground';
