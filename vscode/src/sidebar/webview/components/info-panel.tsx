// Conversation info panel: agents, worktree, branch, spend, and plan —
// shown beside the transcript when the container is wide enough.

import { CheckCircle, Circle, LoaderCircle, XCircle } from 'lucide-react';
import type { ReactNode } from 'react';

import type { PlanStepWire, SubagentSnapshot } from '../../../protocol';
import type { ThreadState } from '../state/bridge';
import { cn } from '../lib/utils';

function formatK(n: number): string {
  return n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n);
}

const Section = ({ title, children }: { title: string; children: ReactNode }) => (
  <section className="space-y-1.5">
    <h3 className="font-medium text-muted-foreground text-[11px] uppercase tracking-wide">
      {title}
    </h3>
    {children}
  </section>
);

const AgentStatusIcon = ({ status }: { status: SubagentSnapshot['status'] }) => {
  if (status === 'running' || status === 'pending-approval') {
    return <LoaderCircle className="size-3.5 shrink-0 animate-spin text-muted-foreground" />;
  }
  if (status === 'success' || status === 'completed' || status === 'continued') {
    return <CheckCircle className="size-3.5 shrink-0 text-green-600" />;
  }
  if (status === 'error' || status === 'failed' || status === 'denied' || status === 'cancelled') {
    return <XCircle className="size-3.5 shrink-0 text-red-600" />;
  }
  return <Circle className="text-muted-foreground size-3.5 shrink-0" />;
};

const PlanStepIcon = ({ status }: { status: PlanStepWire['status'] }) => {
  if (status === 'completed') return <CheckCircle className="size-3.5 shrink-0 text-green-600" />;
  if (status === 'in_progress') {
    return <LoaderCircle className="size-3.5 shrink-0 animate-spin text-muted-foreground" />;
  }
  return <Circle className="text-muted-foreground size-3.5 shrink-0" />;
};

export type InfoPanelProps = {
  thread: ThreadState;
};

export const InfoPanel = ({ thread }: InfoPanelProps) => {
  const info = thread.info;
  const usage = thread.usage;
  const inputTokens =
    (usage?.input_tokens ?? 0) +
    (usage?.cache_creation_input_tokens ?? 0) +
    (usage?.cache_read_input_tokens ?? 0);
  const outputTokens = usage?.output_tokens ?? 0;

  return (
    <aside className="font-chrome w-60 shrink-0 space-y-4 overflow-y-auto border-l p-3 text-xs">
      <Section title="Agents">
        {!info || info.agents.length === 0 ? (
          <p className="text-muted-foreground">None</p>
        ) : (
          <ul className="space-y-1.5">
            {info.agents.map((agent) => (
              <li className="flex items-start gap-1.5" key={agent.id}>
                <AgentStatusIcon status={agent.status} />
                <div className="min-w-0">
                  <p className="truncate font-medium">{agent.agent_type}</p>
                  <p className="text-muted-foreground truncate">{agent.description}</p>
                  {agent.tool_uses > 0 && (
                    <p className="text-muted-foreground">{agent.tool_uses} tool calls</p>
                  )}
                </div>
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section title="Worktree">
        <p className="truncate" title={info?.worktree_path ?? undefined}>
          {info?.worktree_path ? info.worktree_path.split('/').pop() : '—'}
        </p>
      </Section>

      <Section title="Branch">
        <p className="truncate">{thread.branch ?? '…'}</p>
      </Section>

      <Section title="Spend">
        <p>${thread.cost.toFixed(2)}</p>
        {(inputTokens > 0 || outputTokens > 0) && (
          <p className="text-muted-foreground">
            in {formatK(inputTokens)} · out {formatK(outputTokens)}
          </p>
        )}
      </Section>

      <Section title="Plan">
        {!info?.plan || info.plan.steps.length === 0 ? (
          <p className="text-muted-foreground">None</p>
        ) : (
          <>
            {info.plan.explanation && (
              <p className="text-muted-foreground mb-1">{info.plan.explanation}</p>
            )}
            <ul className="space-y-1">
              {info.plan.steps.map((step, index) => (
                <li className="flex items-start gap-1.5" key={index}>
                  <PlanStepIcon status={step.status} />
                  <span
                    className={cn(
                      'min-w-0 flex-1',
                      step.status === 'completed' && 'text-muted-foreground line-through',
                    )}
                  >
                    {step.step}
                  </span>
                </li>
              ))}
            </ul>
          </>
        )}
      </Section>
    </aside>
  );
};
