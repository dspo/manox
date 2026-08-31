// Conversation info card: the captain and sub-agents with live status,
// branch with pending-change counts, a spend tree broken down per model,
// and a static sources section. Rendered as a floating card over the
// transcript's top-right corner on wide containers; positioning belongs to
// the caller.

import {
  Bot,
  CheckCircle,
  ChevronDown,
  ChevronRight,
  Circle,
  GitBranch,
  Minus,
  ShipWheel,
  TriangleAlert,
  XCircle,
} from 'lucide-react';
import { useEffect, useRef, useState, type ReactNode } from 'react';

import type {
  GoalSnapshotWire,
  ModelInfo,
  PlanSnapshotWire,
  PlanStepWire,
  SubagentChildWire,
  SubagentSnapshot,
  TokenUsageSnapshot,
} from '../../../protocol';
import { ThreadApi } from '../api/client';
import { apiTint } from '../lib/api-tint';
import { t } from '../lib/i18n';
import { formatCost, formatTokens, formatTokensPi } from '../lib/usage-format';
import { cn } from '../lib/utils';
import type { ThreadState } from '../state/bridge';
import { BrailleSpinner } from './ui/braille-spinner';

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
    return <BrailleSpinner className="text-accent-foreground" />;
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

/** Localized label for the Goal's status enum (serde snake_case wire form). */
const goalStatusLabel = (status: GoalSnapshotWire['status']): string =>
  t(
    ({
      active: 'goal_active',
      paused: 'goal_paused',
      blocked: 'goal_blocked',
      budget_limited: 'goal_budget_limited',
      complete: 'goal_complete',
    } as const)[status],
  );

/** Small inline actions for the Goal card: pause/resume, clear, edit. */
const GoalActions = ({ goal, sessionId }: { goal: GoalSnapshotWire; sessionId: string }) => {
  const api = new ThreadApi(sessionId);
  const running = goal.status === 'active';
  const edit = () => {
    const objective = window.prompt('Objective', goal.objective);
    if (objective) api.goal('edit', objective, goal.token_budget ?? undefined);
  };
  const btn =
    'text-muted-foreground hover:text-foreground shrink-0 cursor-pointer rounded px-1.5 text-xs';
  return (
    <div className="flex gap-1">
      {running ? (
        <button className={btn} onClick={() => api.goal('pause')} type="button">
          {t('goal_pause')}
        </button>
      ) : (
        <button className={btn} onClick={() => api.goal('resume')} type="button">
          {t('goal_resume')}
        </button>
      )}
      <button className={btn} onClick={edit} type="button">
        {t('goal_edit')}
      </button>
      <button className={btn} onClick={() => api.goal('clear')} type="button">
        {t('goal_clear')}
      </button>
    </div>
  );
};

/** One sub-agent's streamed child events (text/thinking deltas, tool
 * lifecycle) rendered as a collapsible drill-down under the agents list. */
const SubagentMiniPanel = ({ id, events }: { id: string; events: SubagentChildWire[] }) => {
  const [open, setOpen] = useState(false);
  return (
    <div className="mt-0.5">
      <button
        className="text-muted-foreground hover:text-foreground cursor-pointer rounded px-1 text-[11px]"
        onClick={() => setOpen((o) => !o)}
        type="button"
      >
        {open ? t('subagent_hide_activity') : t('subagent_activity')}
      </button>
      {open && (
        <ul className="mt-1 space-y-1 border-l border-border pl-2">
          {events.length === 0 && (
            <li className="text-muted-foreground text-[11px]">…</li>
          )}
          {events.map((ev, i) => (
            <li className="text-muted-foreground text-[11px]" key={i}>
              {ev.kind === 'text' && ev.text}
              {ev.kind === 'thinking' && <span className="italic">{ev.text}</span>}
              {(ev.kind === 'tool_start' || ev.kind === 'tool_end') && (
                <span className="font-code">
                  {ev.kind === 'tool_start' ? '▶' : '■'} {ev.name}
                  {ev.kind === 'tool_end' && ev.is_error ? ' ✗' : ''}
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
};

/** The main agent's own status, mirroring the host's captain indicator:
 * ship-wheel on success (sub-agents get the plain check), a braille spinner
 * while the turn runs or an authorization is pending, and a red X on a
 * failed turn. */
const CaptainStatusIcon = ({ thread }: { thread: ThreadState }) => {
  if (thread.error !== null) {
    return <XCircle className="size-3.5 shrink-0 text-danger" />;
  }
  if (thread.turnActive || (thread.info?.pending_auth_count ?? 0) > 0) {
    return <BrailleSpinner className="text-accent-foreground" />;
  }
  return <ShipWheel className="size-3.5 shrink-0 text-success" />;
};

/** Occupied context for a usage row: live input plus everything cached. */
const usedTokens = (u: TokenUsageSnapshot): number =>
  (u.input_tokens ?? 0) + (u.cache_read_input_tokens ?? 0) + (u.cache_creation_input_tokens ?? 0);

/** Sort key for plan steps: in-progress first, then pending, then completed;
 * the stable sort keeps chronological order within each group. */
const planSortKey = (status: PlanStepWire['status']): number =>
  status === 'in_progress' ? 0 : status === 'pending' ? 1 : 2;

/** One plan step row: status glyph + title, mirroring the host rail's plan
 * rows (the live step is bold foreground, the rest muted). */
const PlanStepRow = ({ step }: { step: PlanStepWire }) => {
  const glyph =
    step.status === 'in_progress' ? (
      <span className="text-foreground min-w-3.5">▶</span>
    ) : step.status === 'completed' ? (
      <span className="text-muted-foreground/70 min-w-3.5">✔</span>
    ) : (
      <span className="text-muted-foreground min-w-3.5">◻</span>
    );
  const titleClass =
    step.status === 'in_progress' ? 'text-foreground font-semibold' : 'text-muted-foreground';
  return (
    <div className="flex items-center gap-1 pl-3 text-xs">
      {glyph}
      <span className={cn('min-w-0 flex-1 truncate', titleClass)}>{step.step}</span>
    </div>
  );
};

/** The model's `UpdatePlan` snapshot as a collapsible task list, mirroring
 * the host rail's plan section: a toggle header with the `done/total` count
 * and sorted steps (in-progress → pending → completed). Collapsed shows the
 * first five steps plus the remaining count, or "All done" when finished. */
const PlanTaskList = ({
  plan,
  collapsed,
  onToggle,
}: {
  plan: PlanSnapshotWire;
  collapsed: boolean;
  onToggle: () => void;
}) => {
  const steps = plan.steps;
  const total = steps.length;
  const done = steps.filter((s) => s.status === 'completed').length;
  const sorted = [...steps].sort((a, b) => planSortKey(a.status) - planSortKey(b.status));
  const shown = Math.min(sorted.length, 5);
  const allDone = done === total;
  return (
    <div>
      <button
        className="flex w-full cursor-pointer items-center gap-1"
        onClick={onToggle}
        type="button"
      >
        {collapsed ? (
          <ChevronRight className="text-muted-foreground size-3.5 shrink-0" />
        ) : (
          <ChevronDown className="text-muted-foreground size-3.5 shrink-0" />
        )}
        <span className="font-code text-muted-foreground/70 ml-auto">
          {done}/{total}
        </span>
      </button>
      {plan.explanation && <p className="text-muted-foreground">{plan.explanation}</p>}
      {collapsed ? (
        allDone ? (
          <p className="text-muted-foreground pl-3">{t('plan_all_done')}</p>
        ) : (
          <>
            {sorted.slice(0, 5).map((s) => (
              <PlanStepRow key={s.step} step={s} />
            ))}
            {(total - done > shown || steps.length > 5) && (
              <p className="text-muted-foreground pl-3">{t('plan_remaining', total - shown)}</p>
            )}
          </>
        )
      ) : (
        <div className="max-h-40 overflow-y-auto">
          {sorted.map((s) => (
            <PlanStepRow key={s.step} step={s} />
          ))}
        </div>
      )}
    </div>
  );
};

/** Full lifetime total of a usage snapshot: occupied context plus output. */
const totalTokens = (u: TokenUsageSnapshot): number => usedTokens(u) + (u.output_tokens ?? 0);

const ModelUsageRow = ({
  modelKey,
  usage,
  cost,
  models,
  last,
  lastUsage,
}: {
  modelKey: string;
  usage: TokenUsageSnapshot;
  cost: number;
  models: ModelInfo[];
  last: boolean;
  /** Latest single request for this model; the budget numerator. */
  lastUsage?: TokenUsageSnapshot;
}) => {
  const model = models.find((m) => `${m.provider}/${m.id}` === modelKey);
  const cap = model?.context_window;
  const used = lastUsage ? usedTokens(lastUsage) : 0;
  const input = usage.input_tokens ?? 0;
  const cacheRead = usage.cache_read_input_tokens ?? 0;
  // Cache-hit share of uncached input plus cache-read; `--` with no input.
  const hitRate =
    input + cacheRead > 0 ? `${((cacheRead / (input + cacheRead)) * 100).toFixed(1)}%` : '--';
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
      {cap !== undefined && lastUsage !== undefined && (
        <p
          className={cn(
            'whitespace-pre overflow-hidden text-ellipsis',
            used / cap >= 0.9 ? 'text-warning' : 'text-muted-foreground',
          )}
        >
          {`${indent}├─ ${Math.min(100, (used / cap) * 100).toFixed(1)}% ${formatTokensPi(used)}/${formatTokensPi(cap)}`}
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
  // Plan task list collapse state. The first plan seen per thread decides
  // the default by length (mirroring the host rail's auto-collapse above
  // five steps); later updates preserve the user's choice, so a long plan
  // never yanks a manually expanded list back closed.
  const [planCollapsed, setPlanCollapsed] = useState(false);
  const planSeen = useRef<Set<string>>(new Set());
  useEffect(() => {
    const plan = info?.plan;
    if (plan && plan.steps.length > 0 && !planSeen.current.has(thread.sessionId)) {
      planSeen.current.add(thread.sessionId);
      setPlanCollapsed(plan.steps.length > 5);
    }
  }, [info?.plan, thread.sessionId]);

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
              <div className="min-w-0 flex-1">
                <p className="truncate font-medium">
                  {agent.agent_type}
                  {agent.description ? ` · ${agent.description}` : ''}
                </p>
                {agent.latest_activity && (
                  <p className="text-muted-foreground truncate">{agent.latest_activity}</p>
                )}
                <SubagentMiniPanel id={agent.id} events={thread.subagentChildren[agent.id] ?? []} />
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

      {info?.cwd_path && (
        <div className="text-muted-foreground flex items-center gap-1.5">
          <GitBranch className="size-3.5 shrink-0 opacity-50" />
          <span className="min-w-0 truncate" title={t('cwd')}>
            {info.cwd_path}
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
          {info?.plan && info.plan.steps.length > 0 && (
            <PlanTaskList
              collapsed={planCollapsed}
              onToggle={() => setPlanCollapsed((c) => !c)}
              plan={info.plan}
            />
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
          <GoalActions goal={info.goal} sessionId={thread.sessionId} />
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
                lastUsage={info?.per_model_last_usage?.[modelKey]}
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
