// Bilingual copy dictionary. The display language is injected by the host
// as a `vscode-language` meta tag; a `zh` prefix selects Chinese, anything
// else English. Components take every user-facing string from `t` — inline
// copy is forbidden so the two locales cannot drift.

type Entry = {
  en: string | ((...args: number[]) => string);
  zh: string | ((...args: number[]) => string);
};

const DICT = {
  conversation_info: { en: 'Conversation info', zh: '对话信息' },
  agents: { en: 'Agents', zh: '智能体' },
  captain: { en: 'Captain', zh: '船长' },
  branch: { en: 'Branch', zh: '分支' },
  spend: { en: 'Spend', zh: '消费' },
  sources: { en: 'Sources', zh: '来源' },
  no_sources: { en: 'No sources yet', zh: '暂无来源' },
  autopilot: { en: 'AutoPilot', zh: '自动驾驶' },
  danger: { en: 'Danger', zh: '危险驾驶' },
  autopilot_desc: {
    en: 'Tools run after an automatic safety review',
    zh: '工具经自动安全审查后执行',
  },
  danger_desc: { en: 'Every tool call runs without prompting', zh: '所有工具调用直接执行' },
  approval_mode: { en: 'Approval mode', zh: '审批模式' },
  composer_placeholder: {
    en: 'Type a message, then send to begin',
    zh: '输入消息，点击发送以开始使用',
  },
  starting_session: { en: 'Starting session…', zh: '正在启动会话…' },
  no_model_configured: { en: 'No model configured', zh: '未配置模型' },
  no_models_configured: { en: 'No models configured', zh: '未配置模型' },
  you: { en: 'You', zh: '你' },
  no_messages_title: { en: 'No messages yet', zh: '暂无消息' },
  no_messages_desc: { en: 'Send a message to start', zh: '发送消息以开始' },
  threads_empty: { en: 'No conversations yet', zh: '暂无对话' },
  sessions: { en: 'Sessions', zh: '会话' },
  archive: { en: 'Archive', zh: '归档' },
  unarchive: { en: 'Unarchive', zh: '取消归档' },
  pin: { en: 'Pin', zh: '置顶' },
  unpin: { en: 'Unpin', zh: '取消置顶' },
  more: { en: 'More', zh: '更多' },
  back_to_threads: { en: 'Back to threads', zh: '返回对话列表' },
  copy: { en: 'Copy', zh: '复制' },
  send: { en: 'Send', zh: '发送' },
  stop: { en: 'Stop', zh: '停止' },
  remove_attachment: { en: 'Remove attachment', zh: '移除附件' },
  context_compacted: { en: 'context compacted', zh: '上下文已压缩' },
  thinking: { en: 'Thinking…', zh: '思考中…' },
  thought_seconds: {
    en: (s: number) => `Thought for ${s} seconds`,
    zh: (s: number) => `思考了 ${s} 秒`,
  },
  thought_brief: { en: 'Thought for a few seconds', zh: '思考了几秒' },
  thought_n_turns: {
    en: (n: number) => `thought for ${n} ${n === 1 ? 'round' : 'rounds'}`,
    zh: (n: number) => `思考了 ${n} 轮次`,
  },
  called_n_tools: {
    en: (n: number) => `${n} tool ${n === 1 ? 'call' : 'calls'}`,
    zh: (n: number) => `调用了 ${n} 次工具`,
  },
  duration_seconds: { en: (s: number) => `${s}s`, zh: (s: number) => `${s} 秒` },
  show_n_more: { en: (n: number) => `+${n} more`, zh: (n: number) => `还有 ${n} 行` },
  // Built-in slash-command descriptions, keyed by the agent locales' fluent
  // keys so the actor's `CommandEntry.i18n_key` maps straight into the dict.
  'slash-danger-desc': {
    en: 'Switch to Danger (no approvals + bash outside sandbox); with a prompt, switches and starts working immediately',
    zh: '切换到危险驾驶（免审批 + bash 沙箱外）；带提示词则切换后直接开工',
  },
  'slash-plan-desc': {
    en: 'Toggle plan mode (read-only research, plan file, structured approval); `/plan <prompt>` enters plan mode and starts planning the prompt',
    zh: '切换 plan 模式（只读调研、plan 文件、结构化批准）；`/plan <提示>` 进入 plan 模式并开始规划该提示',
  },
  'slash-compact-desc': {
    en: 'Compact the conversation: summarize older history into a handoff note so the thread can keep going past the context limit',
    zh: '压缩对话：把较早的历史摘要成一份交接说明，让会话越过上下文上限继续进行',
  },
  'slash-exit-desc': {
    en: 'Archive the current thread and start a fresh one',
    zh: '归档当前会话并开始一个新会话',
  },
  'slash-new-desc': {
    en: 'Archive the current thread and start a fresh one that keeps the project, approval mode, and model',
    zh: '归档当前会话并开始新会话，保留项目、驾驶模式与模型',
  },
  'slash-goal-desc': {
    en: 'Create or manage a persistent Goal (`/goal <objective>`, pause, resume, edit, clear)',
    zh: '创建或管理持久目标（`/goal <目标>`、pause、resume、edit、clear）',
  },
  plan: { en: 'Plan', zh: '计划' },
  plan_mode: { en: 'Plan mode', zh: '计划模式' },
  plan_mode_on: { en: 'On', zh: '已开启' },
  plan_mode_off: { en: 'Off', zh: '已关闭' },
  worktree: { en: 'Worktree', zh: '工作树' },
  goal: { en: 'Goal', zh: '目标' },
  goal_active: { en: 'Active', zh: '进行中' },
  goal_paused: { en: 'Paused', zh: '已暂停' },
  goal_blocked: { en: 'Blocked', zh: '受阻' },
  goal_budget_limited: { en: 'Budget limited', zh: '预算受限' },
  goal_complete: { en: 'Complete', zh: '已完成' },
} satisfies Record<string, Entry>;

export type I18nKey = keyof typeof DICT;

let language: string | null = null;

function detectLanguage(): string {
  if (language !== null) return language;
  language =
    (typeof document !== 'undefined' &&
      document.querySelector<HTMLMetaElement>('meta[name="vscode-language"]')?.content) ||
    'en';
  return language;
}

/** Translate a dictionary key; numeric arguments feed interpolation. */
export function t(key: I18nKey, ...args: number[]): string {
  const variant = detectLanguage().startsWith('zh') ? 'zh' : 'en';
  const value = DICT[key][variant];
  return typeof value === 'function'
    ? (value as (...a: number[]) => string)(...args)
    : value;
}

/** Whether a key exists in the dict — unknown actor-shipped keys (a built-in
 * added on the Rust side before this webview build) fall back to the raw
 * description instead of throwing. */
export function hasCommandKey(key: string): boolean {
  return key in DICT;
}

const relativeFormatters = new Map<string, Intl.RelativeTimeFormat>();

/** Relative wall-clock distance ("3 minutes ago" / "3 分钟前"), following
 * the display language. */
export function formatRelativeTime(unixSeconds: number): string {
  const locale = detectLanguage().startsWith('zh') ? 'zh' : 'en';
  let rtf = relativeFormatters.get(locale);
  if (!rtf) {
    rtf = new Intl.RelativeTimeFormat(locale, { numeric: 'auto' });
    relativeFormatters.set(locale, rtf);
  }
  const diff = unixSeconds - Date.now() / 1000;
  const abs = Math.abs(diff);
  if (abs < 60) return rtf.format(Math.round(diff), 'second');
  if (abs < 3_600) return rtf.format(Math.round(diff / 60), 'minute');
  if (abs < 86_400) return rtf.format(Math.round(diff / 3_600), 'hour');
  return rtf.format(Math.round(diff / 86_400), 'day');
}

/** Test seam: pin the display language without a meta tag. */
export function setLanguageForTest(lang: string): void {
  language = lang;
}
