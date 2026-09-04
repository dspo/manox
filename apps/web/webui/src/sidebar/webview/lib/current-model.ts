import type { ModelInfo } from '../../../protocol';

/** Resolve the picker's currently-selected model entry from a canonical
 * wire ref (L8). The ref is the `model` projection's `{provider, modelId}`
 * joined as `{provider}/{modelId}` (the same form the picker sends on
 * select); match is an exact comparison against the canonical identity —
 * never a bare-id first-match (that resolution is server-only, and the
 * old fallback mis-attributed same-bare-id registrations). */
export const findCurrentModel = (
  models: ModelInfo[],
  currentModelRef: string | null,
): ModelInfo | undefined =>
  currentModelRef === null
    ? undefined
    : models.find((m) => `${m.provider}/${m.id}` === currentModelRef);
