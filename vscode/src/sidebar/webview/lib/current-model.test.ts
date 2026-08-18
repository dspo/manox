import { describe, expect, it } from 'vitest';

import type { ModelInfo } from '../../../protocol';
import { findCurrentModel } from './current-model';

const models: ModelInfo[] = [
  {
    id: 'deepseek-v4-pro',
    name: 'deepseek-v4-pro',
    provider: 'DeepSeek-anthropic',
    provider_name: 'DeepSeek',
    api: 'anthropic',
    context_window: 200000,
  },
  {
    id: 'deepseek-v4-pro',
    name: 'deepseek-v4-pro',
    provider: 'DeepSeek-responses',
    provider_name: 'DeepSeek',
    api: 'openai_responses',
    context_window: 200000,
  },
];

describe('findCurrentModel', () => {
  it('matches the registration-qualified id a draft selection stores', () => {
    const current = findCurrentModel(models, 'DeepSeek-responses/deepseek-v4-pro');
    expect(current?.provider).toBe('DeepSeek-responses');
  });

  it('matches the bare id a live session reports', () => {
    // Both variants share the bare id; the first listing wins, mirroring the
    // host's wire form.
    const current = findCurrentModel(models, 'deepseek-v4-pro');
    expect(current?.provider).toBe('DeepSeek-anthropic');
  });

  it('returns undefined for null or unknown ids', () => {
    expect(findCurrentModel(models, null)).toBeUndefined();
    expect(findCurrentModel(models, 'no-such-model')).toBeUndefined();
    expect(findCurrentModel([], 'deepseek-v4-pro')).toBeUndefined();
  });
});
