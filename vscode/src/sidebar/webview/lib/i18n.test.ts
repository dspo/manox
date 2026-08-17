import { afterEach, describe, expect, it, vi } from 'vitest';

import { formatRelativeTime, setLanguageForTest, t } from './i18n';

describe('i18n', () => {
  afterEach(() => {
    setLanguageForTest('en');
    vi.useRealTimers();
  });

  it('translates built-in slash descriptions and info labels in both locales', () => {
    setLanguageForTest('zh-cn');
    expect(t('slash-plan-desc')).toContain('plan');
    expect(t('slash-danger-desc')).toContain('危险');
    expect(t('goal_complete')).toBe('已完成');
    setLanguageForTest('en');
    expect(t('slash-plan-desc')).toContain('plan mode');
    expect(t('slash-new-desc')).toContain('fresh one');
    expect(t('goal_paused')).toBe('Paused');
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

  it('localizes the plan-mode banner in both locales', () => {
    setLanguageForTest('zh-cn');
    expect(t('plan_mode_banner')).toBe('计划模式已开启');
    expect(t('plan_mode_exit')).toBe('退出计划模式');
    setLanguageForTest('en');
    expect(t('plan_mode_banner')).toBe('Plan mode is on');
    expect(t('plan_mode_exit')).toBe('Exit plan mode');
  });

  it('degrades unknown keys to the raw key instead of throwing', () => {
    // Wire-driven lookups can name keys this build never shipped; the
    // render tree must survive them.
    expect(t('not_a_real_key' as never)).toBe('not_a_real_key');
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

  it('formats relative time in the display language', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-01-15T12:00:00Z'));
    const now = Date.now() / 1000;
    setLanguageForTest('zh-cn');
    expect(formatRelativeTime(now - 90)).toBe('1分钟前');
    setLanguageForTest('en');
    expect(formatRelativeTime(now - 90)).toBe('1 minute ago');
  });

  it('picks the relative-time unit from the distance', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-01-15T12:00:00Z'));
    const now = Date.now() / 1000;
    setLanguageForTest('en');
    expect(formatRelativeTime(now - 30)).toBe('30 seconds ago');
    expect(formatRelativeTime(now - 7_200)).toBe('2 hours ago');
    expect(formatRelativeTime(now - 3 * 86_400)).toBe('3 days ago');
  });
});
