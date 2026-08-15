// Conversation info card: agents, branch with pending-change counts, a
// spend tree broken down per model, and a static sources section. Rendered
// as a floating card over the transcript's top-right corner on wide
// containers; positioning belongs to the caller.

import { CheckCircle, Circle, GitBranch, LoaderCircle, XCircle } from 'lucide-react';
import type { ReactNode } from 'react';

import type { ModelInfo, SubagentSnapshot, TokenUsageSnapshot } from '../../../protocol';
import { t } from '../lib/i18n';
import { cn } from '../lib/utils';
import type { ThreadState } from '../state/bridge';

const formatTokens = (n: number): string =>
  n >= 1_000_000
    ? `${(n / 1_000_000).toFixed(1)}m`
    : n >= 1000
      ? `${(n / 1000).toFixed(1)}k`
      : String(n);

const Section = ({ title, children }: { title: string; children: ReactNode }) => (
  <section className="space-y-1.5">
    <h3 className="font-medium text-muted-foreground text-[11px] tracking-wide">{title}</h3>
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
  if (status === 'error' || status === 'failed' || status === 'denied' || status === 'cancelled') {
    return <XCircle className="size-3.5 shrink-0 text-danger" />;
  }
  return <Circle className="text-muted-foreground size-3.5 shrink-0" />;
};

/** Occupied context for a usage row: live input plus everything cached. */
const usedTokens = (u: TokenUsageSnapshot): number =>
  (u.input_tokens ?? 0) + (u.cache_read_input_tokens ?? 0) + (u.cache_creation_input_tokens ?? 0);

const ModelUsageRow = ({
  modelKey,
  usage,
  models,
  last,
}: {
  modelKey: string;
  usage: TokenUsageSnapshot;
  models: ModelInfo[];
  last: boolean;
}) => {
  const cap = models.find((m) => `${m.provider}/${m.id}` === modelKey)?.context_window;
  const used = usedTokens(usage);
  const input = (usage.input_tokens ?? 0) + (usage.cache_creation_input_tokens ?? 0);
  const cacheRead = usage.cache_read_input_tokens ?? 0;
  const hitRate =
    input + cacheRead > 0 ? Math.round((cacheRead / (input + cacheRead)) * 100) : null;
  return (
    <div className="font-code text-[11px]">
      <p className="truncate" title={modelKey}>
        <span className="text-muted-foreground">{last ? '└─' : '├─'}</span> {modelKey}
      </p>
      <div className="text-muted-foreground pl-5">
        {cap ? <p>{Math.round((used / cap) * 100)}% / {formatTokens(cap)}</p> : null}
        <p>
          ↑{formatTokens(input)} ↓{formatTokens(usage.output_tokens ?? 0)} R
          {formatTokens(cacheRead)}
          {hitRate !== null && ` CH${hitRate}%`}
        </p>
      </div>
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
  const totalTokens = usage ? usedTokens(usage) + (usage.output_tokens ?? 0) : 0;
  const gitStats = info?.git_stats;
  const perModel = Object.entries(info?.per_model_usage ?? {});

  return (
    <aside
      className={cn(
        'font-chrome bg-card flex flex-col gap-3 rounded-lg border p-3 text-xs',
        'shadow-[-3px_6px_10px_rgba(0,0,0,0.22)]',
        className,
      )}
    >
      <h2 className="font-medium text-sm">{t('conversation_info')}</h2>

      <Section title={t('agents')}>
        {!info || info.agents.length === 0 ? (
          <p className="text-muted-foreground">{t('agents_none')}</p>
        ) : (
          <ul className="space-y-1.5">
            {info.agents.map((agent) => (
              <li className="flex items-start gap-1.5" key={agent.id}>
                <AgentStatusIcon status={agent.status} />
                <div className="min-w-0">
                  <p className="truncate font-medium">{agent.agent_type}</p>
                  {agent.latest_activity && (
                    <p className="text-muted-foreground truncate">{agent.latest_activity}</p>
                  )}
                </div>
              </li>
            ))}
          </ul>
        )}
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

      <Section title={t('spend')}>
        <div className="flex items-baseline justify-between">
          <span className="text-muted-foreground">${thread.cost.toFixed(2)}</span>
          <span>{formatTokens(totalTokens)}</span>
        </div>
        {perModel.length > 0 && (
          <div className="space-y-1">
            {perModel.map(([modelKey, modelUsage], index) => (
              <ModelUsageRow
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
