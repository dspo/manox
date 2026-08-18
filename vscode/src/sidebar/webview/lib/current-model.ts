import type { ModelInfo } from '../../../protocol';

/** Resolve the picker's currently-selected model entry. Draft threads carry
 * the registration-qualified `provider/id` the picker sends on select; live
 * sessions report the bare model id on the wire — accept both. */
export const findCurrentModel = (
  models: ModelInfo[],
  currentModelId: string | null,
): ModelInfo | undefined =>
  currentModelId === null
    ? undefined
    : models.find(
        (m) => `${m.provider}/${m.id}` === currentModelId || m.id === currentModelId,
      );
