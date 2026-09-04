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
  it('matches the registration-qualified canonical ref', () => {
    const current = findCurrentModel(models, 'DeepSeek-responses/deepseek-v4-pro');
    expect(current?.provider).toBe('DeepSeek-responses');
  });
  it('matches the first registration by its own canonical ref', () => {
    const current = findCurrentModel(models, 'DeepSeek-anthropic/deepseek-v4-pro');
    expect(current?.provider).toBe('DeepSeek-anthropic');
  });
  it('never resolves a bare id to a first match (L8 canonical-only)', () => {
    expect(findCurrentModel(models, 'deepseek-v4-pro')).toBeUndefined();
  });
  it('returns undefined for null or unknown refs', () => {
    expect(findCurrentModel(models, null)).toBeUndefined();
    expect(findCurrentModel(models, 'no-such-provider/deepseek-v4-pro')).toBeUndefined();
    expect(findCurrentModel([], 'DeepSeek-anthropic/deepseek-v4-pro')).toBeUndefined();
  });
});
