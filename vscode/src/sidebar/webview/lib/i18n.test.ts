import { afterEach, describe, expect, it } from 'vitest';

import { setLanguageForTest, t } from './i18n';

describe('i18n', () => {
  afterEach(() => {
    setLanguageForTest('en');
  });

  it('serves Chinese for zh display languages', () => {
    setLanguageForTest('zh-cn');
    expect(t('conversation_info')).toBe('对话信息');
    expect(t('no_sources')).toBe('暂无来源');
    expect(t('autopilot')).toBe('自动驾驶');
    expect(t('danger')).toBe('危险驾驶');
  });

  it('falls back to English otherwise', () => {
    setLanguageForTest('en');
    expect(t('conversation_info')).toBe('Conversation info');
    setLanguageForTest('de');
    expect(t('conversation_info')).toBe('Conversation info');
  });

  it('interpolates numeric arguments', () => {
    setLanguageForTest('zh-cn');
    expect(t('thought_n_turns', 3)).toBe('思考了 3 轮次');
    expect(t('called_n_tools', 2)).toBe('调用了 2 次工具');
    setLanguageForTest('en');
    expect(t('thought_n_turns', 1)).toBe('thought for 1 round');
    expect(t('called_n_tools', 2)).toBe('2 tool calls');
    expect(t('show_n_more', 12)).toBe('+12 more');
  });
});
