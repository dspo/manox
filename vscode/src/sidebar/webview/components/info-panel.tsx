// Conversation info card: the captain and sub-agents with live status,
// branch with pending-change counts, a spend tree broken down per model,
// and a static sources section. Rendered as a floating card over the
// transcript's top-right corner on wide containers; positioning belongs to
// the caller.

import {
  Bot,
  CheckCircle,
  Circle,
  GitBranch,
  LoaderCircle,
  Minus,
  ShipWheel,
  TriangleAlert,
  XCircle,
} from 'lucide-react';
import type { ReactNode } from 'react';

import type { GoalSnapshotWire, ModelInfo, SubagentSnapshot, TokenUsageSnapshot } from '../../../protocol';
import { apiTint } from '../lib/api-tint';
import { t } from '../lib/i18n';
import { formatCost, formatTokens, formatTokensPi } from '../lib/usage-format';
import { cn } from '../lib/utils';
import type { ThreadState } from '../state/bridge';

/** Spend-section glyph, matching the host's scorpio mark. */
const ScorpioIcon = ({ className }: { className?: string }) => (
  <svg
    className={className}
    fill="none"
    stroke="currentColor"
    strokeLinecap="round"
    strokeLinejoin="round"
    strokeWidth={2}
    viewBox="0 0 24 24"
  >
    <path d="M10 19V5.5a1 1 0 0 1 5 0V17a2 2 0 0 0 2 2h5l-3-3" />
    <path d="m22 19-3 3" />
    <path d="M5 19V5.5a1 1 0 0 1 5 0" />
    <path d="M5 5.5A2.5 2.5 0 0 0 2.5 3" />
  </svg>
);

const Section = ({
  title,
  icon,
  trailing,
  children,
}: {
  title: string;
  icon?: ReactNode;
  trailing?: ReactNode;
  children: ReactNode;
}) => (
  <section className="space-y-1.5">
    <h3 className="font-medium text-muted-foreground flex items-center gap-1.5 text-[11px] tracking-wide">
      {icon}
      <span>{title}</span>
      {trailing !== undefined && (
        <span className="font-code text-foreground ml-auto font-normal">{trailing}</span>
      )}
    </h3>
    {children}
  </section>
);

const AgentStatusIcon = ({ status }: { status: SubagentSnapshot['status'] }) => {
  if (status === 'running' || status === 'pending-approval') {
    return <LoaderCircle className="size-3.5 shrink-0 animate-spin text-muted-foreground" />;
  }
  if (status === 'success' || status === 'completed' || status === 'continued') {
    return <CheckCircle className="size-3.5 shrink-0 text-success" />;
  }
  if (status === 'error' || status === 'failed' || status === 'denied') {
    return <XCircle className="size-3.5 shrink-0 text-danger" />;
  }
  if (status === 'cancelled') {
    return <Minus className="text-muted-foreground size-3.5 shrink-0" />;
  }
  return <Circle className="text-muted-foreground size-3.5 shrink-0" />;
};

/** Localized label for the Goal's status enum (serde PascalCase wire form). */
const goalStatusLabel = (status: GoalSnapshotWire['status']): string =>
  t(
    ({
      Active: 'goal_active',
      Paused: 'goal_paused',
      Blocked: 'goal_blocked',
      BudgetLimited: 'goal_budget_limited',
      Complete: 'goal_complete',
    } as const)[status],
  );

/** The main agent's own status, mirroring the host's captain indicator:
 * ship-wheel on success (sub-agents get the plain check) and a red X on a
 * failed turn. */
const CaptainStatusIcon = ({ thread }: { thread: ThreadState }) => {
  if (thread.turnActive) {
    return <LoaderCircle className="size-3.5 shrink-0 animate-spin text-muted-foreground" />;
  }
  if (thread.error !== null) {
    return <XCircle className="size-3.5 shrink-0 text-danger" />;
  }
  if ((thread.info?.pending_auth_count ?? 0) > 0) {
    return <TriangleAlert className="text-warning size-3.5 shrink-0" />;
  }
  return <ShipWheel className="size-3.5 shrink-0 text-success" />;
};

/** Occupied context for a usage row: live input plus everything cached. */
const usedTokens = (u: TokenUsageSnapshot): number =>
  (u.input_tokens ?? 0) + (u.cache_read_input_tokens ?? 0) + (u.cache_creation_input_tokens ?? 0);

/** Full lifetime total of a usage snapshot: occupied context plus output. */
const totalTokens = (u: TokenUsageSnapshot): number => usedTokens(u) + (u.output_tokens ?? 0);

const ModelUsageRow = ({
  modelKey,
  usage,
  cost,
  models,
  last,
}: {
  modelKey: string;
  usage: TokenUsageSnapshot;
  cost: number;
  models: ModelInfo[];
  last: boolean;
}) => {
  const model = models.find((m) => `${m.provider}/${m.id}` === modelKey);
  const cap = model?.context_window;
  const used = usedTokens(usage);
  const input = usage.input_tokens ?? 0;
  const cacheRead = usage.cache_read_input_tokens ?? 0;
  // Cache-hit share of uncached input plus cache-read; `--` with no input.
  const hitRate =
    input + cacheRead > 0 ? ((cacheRead / (input + cacheRead)) * 100).toFixed(1) : '--';
  const branch = last ? '└─' : '├─';
  const indent = last ? '    ' : '│   ';
  return (
    <div className="font-code text-[11px]">
      <p className="truncate" title={modelKey}>
        {model ? (
          <>
            {branch}{' '}
            <span className="text-muted-foreground">{model.provider_name ?? model.provider}/</span>
            <span className={apiTint(model.api)}>{model.name}</span>
          </>
        ) : (
          `${branch} ${modelKey}`
        )}
      </p>
      {cap !== undefined && (
        <p
          className={cn(
            'whitespace-pre overflow-hidden text-ellipsis',
            used / cap >= 0.9 ? 'text-warning' : 'text-muted-foreground',
          )}
        >
          {`${indent}├─ ${((used / cap) * 100).toFixed(1)}% ${formatTokensPi(used)}/${formatTokensPi(cap)}`}
        </p>
      )}
      <p className="text-muted-foreground whitespace-pre overflow-hidden text-ellipsis">
        {`${indent}${cost > 0 ? '├─' : '└─'} ↑${formatTokensPi(input)} ↓${formatTokensPi(usage.output_tokens ?? 0)} R${formatTokensPi(cacheRead)} CH${hitRate}`}
      </p>
      {cost > 0 && (
        <p className="text-muted-foreground whitespace-pre overflow-hidden text-ellipsis">{`${indent}└─ ${formatCost(cost)}`}</p>
      )}
    </div>
  );
};

export type InfoPanelProps = {
  thread: ThreadState;
  models: ModelInfo[];
  className?: string;
};

export const InfoPanel = ({ thread, models, className }: InfoPanelProps) => {
  const info = thread.info;
  const usage = thread.usage;
  const total = usage ? totalTokens(usage) : 0;
  const gitStats = info?.git_stats;
  // Same ordering as the host rail: heaviest spenders first.
  const perModel = Object.entries(info?.per_model_usage ?? {}).sort(
    ([, a], [, b]) => totalTokens(b) - totalTokens(a),
  );

  return (
    <aside
      className={cn(
        'font-chrome bg-card flex flex-col gap-3 rounded-lg border border-border p-3 text-xs',
        'shadow-[-3px_6px_10px_rgba(0,0,0,0.22)]',
        className,
      )}
    >
      <h2 className="font-medium text-sm">{t('conversation_info')}</h2>

      <Section icon={<Bot className="size-3.5" />} title={t('agents')}>
        <ul className="space-y-1.5">
          <li className="flex items-start gap-1.5">
            <CaptainStatusIcon thread={thread} />
            <div className="min-w-0">
              <p className="truncate font-medium">{t('captain')}</p>
            </div>
          </li>
          {(info?.agents ?? []).map((agent) => (
            <li className="flex items-start gap-1.5" key={agent.id}>
              <AgentStatusIcon status={agent.status} />
              <div className="min-w-0">
                <p className="truncate font-medium">
                  {agent.agent_type}
                  {agent.description ? ` · ${agent.description}` : ''}
                </p>
                {agent.latest_activity && (
                  <p className="text-muted-foreground truncate">{agent.latest_activity}</p>
                )}
              </div>
            </li>
          ))}
        </ul>
      </Section>

      <Section title={t('branch')}>
        <div className="flex items-center gap-1.5">
          <GitBranch className="size-3.5 shrink-0 text-muted-foreground" />
          <span className="min-w-0 truncate">{thread.branch ?? '…'}</span>
          {gitStats && (
            <span className="font-code ml-auto flex shrink-0 gap-1.5">
              <span className="text-success">+{gitStats.added}</span>
              <span className="text-danger">-{gitStats.deleted}</span>
              <span className="text-muted-foreground">?{gitStats.untracked}</span>
            </span>
          )}
        </div>
      </Section>

      {info?.worktree_path && (
        <div className="text-muted-foreground flex items-center gap-1.5">
          <GitBranch className="size-3.5 shrink-0 opacity-50" />
          <span className="min-w-0 truncate" title={t('worktree')}>
            {info.worktree_path}
          </span>
        </div>
      )}

      {(thread.planMode || info?.plan) && (
        <Section title={t('plan')}>
          <p className="flex items-center gap-1.5">
            <span className="text-muted-foreground">{t('plan_mode')}</span>
            <span className={cn('ml-auto font-medium', thread.planMode ? 'text-info' : '')}>
              {t(thread.planMode ? 'plan_mode_on' : 'plan_mode_off')}
            </span>
          </p>
          {info?.plan && (
            <p className="text-muted-foreground">
              {info.plan.explanation}
              <span className="font-code ml-1">
                {info.plan.steps.filter((s) => s.status === 'completed').length}/
                {info.plan.steps.length}
              </span>
            </p>
          )}
        </Section>
      )}

      {info?.goal && (
        <Section title={t('goal')}>
          <p className="flex items-start gap-1.5">
            <span className="min-w-0 flex-1">{info.goal.objective}</span>
            <span className="text-muted-foreground shrink-0">{goalStatusLabel(info.goal.status)}</span>
          </p>
          {info.goal.token_budget !== null && (
            <p className="text-muted-foreground">
              {formatTokensPi(info.goal.token_budget)}
            </p>
          )}
        </Section>
      )}

      <Section
        icon={<ScorpioIcon className="size-3.5" />}
        title={t('spend')}
        trailing={
          <>
            {formatTokens(total)}
            {thread.cost > 0 && ` · ${formatCost(thread.cost)}`}
          </>
        }
      >
        {perModel.length > 0 && (
          <div className="space-y-1">
            {perModel.map(([modelKey, modelUsage], index) => (
              <ModelUsageRow
                cost={info?.per_model_cost?.[modelKey] ?? 0}
                key={modelKey}
                last={index === perModel.length - 1}
                models={models}
                modelKey={modelKey}
                usage={modelUsage}
              />
            ))}
          </div>
        )}
      </Section>

      <div className="border-border border-t" />

      <Section title={t('sources')}>
        <p className="text-muted-foreground">{t('no_sources')}</p>
      </Section>
    </aside>
  );
};
