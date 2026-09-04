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
  ConversationInfo,
  ConversationModelRow,
  GoalSnapshotWire,
  ModelInfo,
  PlanSnapshotWire,
  PlanStepWire,
  SubagentChildWire,
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

const AgentStatusIcon = ({ status }: { status: string }) => {
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
  if (thread.turnActive || thread.pendingAuthIds.length > 0) {
    return <BrailleSpinner className="text-accent-foreground" />;
  }
  return <ShipWheel className="size-3.5 shrink-0 text-success" />;
};

/** Occupied context for a §E.3 per-model row: the last request's full
 * context numerator (input incl. cache classes + output). */
const rowUsed = (row: ConversationModelRow): number => row.lastTotal;

/** Lifetime input for a §E.3 row: uncached input only (cache reads are
 * billed separately in the ↑/R columns). */
const rowInput = (row: ConversationModelRow): number => row.input;

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

/** Full lifetime total of a §E.3 fold: everything across all model rows. */
const infoTotal = (info: ConversationInfo): number =>
  info.models.reduce(
    (sum, row) => sum + row.input + row.output + row.cacheRead + row.cacheWrite,
    0,
  );

const ModelUsageRow = ({
  row,
  models,
  last,
}: {
  row: ConversationModelRow;
  models: ModelInfo[];
  last: boolean;
}) => {
  // Canonical wire identity (L8): `{provider}/{model}`; exact match only.
  const model = models.find((m) => `${m.provider}/${m.id}` === row.model);
  const cap = row.contextWindow ?? model?.context_window;
  const used = rowUsed(row);
  const input = rowInput(row);
  const cacheRead = row.cacheRead;
  // Cache-hit share of uncached input plus cache-read; `--` with no input.
  const hitRate =
    input + cacheRead > 0 ? `${((cacheRead / (input + cacheRead)) * 100).toFixed(1)}%` : '--';
  const branch = last ? '└─' : '├─';
  const indent = last ? '    ' : '│   ';
  return (
    <div className="font-code text-[11px]">
      <p className="truncate" title={row.model}>
        {model ? (
          <>
            {branch}{' '}
            <span className="text-muted-foreground">{model.provider_name ?? model.provider}/</span>
            <span className={apiTint(model.api)}>{model.name}</span>
          </>
        ) : (
          `${branch} ${row.model}`
        )}
      </p>
      {cap !== undefined && cap > 0 && row.lastTotal > 0 && (
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
        {`${indent}└─ ↑${formatTokensPi(input)} ↓${formatTokensPi(row.output)} R${formatTokensPi(cacheRead)} CH${hitRate}`}
      </p>
    </div>
  );
};

export type InfoPanelProps = {
  thread: ThreadState;
  models: ModelInfo[];
  className?: string;
};

export const InfoPanel = ({ thread, models, className }: InfoPanelProps) => {
  // v2 (§E.3): the spend tree / lifetime totals come from the Q-face
  // `GetConversationInfo` fold; plan/goal/cwd/title come from the P-face
  // projections on `ThreadState`. The doomed ThreadInfo/UsageSnapshot
  // composite is gone.
  const info = thread.conversationInfo;
  const total = info ? infoTotal(info) : 0;
  const git = info?.git ?? null;
  // Same ordering as the host rail: heaviest spenders first.
  const perModel = [...(info?.models ?? [])].sort(
    (a, b) => b.input + b.output + b.cacheRead - (a.input + a.output + a.cacheRead),
  );
  // Plan task list collapse state. The first plan seen per thread decides
  // the default by length (mirroring the host rail's auto-collapse above
  // five steps); later updates preserve the user's choice, so a long plan
  // never yanks a manually expanded list back closed.
  const [planCollapsed, setPlanCollapsed] = useState(false);
  const planSeen = useRef<Set<string>>(new Set());
  useEffect(() => {
    const plan = thread.plan;
    if (plan && plan.steps.length > 0 && !planSeen.current.has(thread.sessionId)) {
      planSeen.current.add(thread.sessionId);
      setPlanCollapsed(plan.steps.length > 5);
    }
  }, [thread.plan, thread.sessionId]);

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
          {thread.subagents.map((agent) => (
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
          <span className="min-w-0 truncate">{git?.branch ?? thread.branch ?? '…'}</span>
          {git && (
            <span className="font-code ml-auto flex shrink-0 gap-1.5">
              {git.ahead > 0 && <span className="text-success">↑{git.ahead}</span>}
              {git.behind > 0 && <span className="text-danger">↓{git.behind}</span>}
              {git.dirty > 0 && <span className="text-muted-foreground">~{git.dirty}</span>}
            </span>
          )}
        </div>
      </Section>

      {thread.cwd && (
        <div className="text-muted-foreground flex items-center gap-1.5">
          <GitBranch className="size-3.5 shrink-0 opacity-50" />
          <span className="min-w-0 truncate" title={t('cwd')}>
            {thread.cwd}
          </span>
        </div>
      )}

      {(thread.planMode || thread.plan) && (
        <Section title={t('plan')}>
          <p className="flex items-center gap-1.5">
            <span className="text-muted-foreground">{t('plan_mode')}</span>
            <span className={cn('ml-auto font-medium', thread.planMode ? 'text-info' : '')}>
              {t(thread.planMode ? 'plan_mode_on' : 'plan_mode_off')}
            </span>
          </p>
          {thread.plan && thread.plan.steps.length > 0 && (
            <PlanTaskList
              collapsed={planCollapsed}
              onToggle={() => setPlanCollapsed((c) => !c)}
              plan={thread.plan}
            />
          )}
        </Section>
      )}

      {thread.goal && (
        <Section title={t('goal')}>
          <p className="flex items-start gap-1.5">
            <span className="min-w-0 flex-1">{thread.goal.objective}</span>
            <span className="text-muted-foreground shrink-0">{goalStatusLabel(thread.goal.status)}</span>
          </p>
          {thread.goal.token_budget !== null && (
            <p className="text-muted-foreground">
              {formatTokensPi(thread.goal.token_budget)}
            </p>
          )}
          <GoalActions goal={thread.goal} sessionId={thread.sessionId} />
        </Section>
      )}

      <Section
        icon={<ScorpioIcon className="size-3.5" />}
        title={t('spend')}
        trailing={
          <>
            {formatTokens(total)}
            {info && info.cumulativeCost > 0 && ` · ${formatCost(info.cumulativeCost)}`}
          </>
        }
      >
        {perModel.length > 0 && (
          <div className="space-y-1">
            {perModel.map((row, index) => (
              <ModelUsageRow
                key={row.model}
                last={index === perModel.length - 1}
                models={models}
                row={row}
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
