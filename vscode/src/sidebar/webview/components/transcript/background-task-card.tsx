// Background-task card: a live mirror of one Monitor/background-Bash task.
// Streams the bounded output tail and offers a Stop button for running tasks.

import { useState } from 'react';

import type { BackgroundTaskSnapshotWire } from '../../../../protocol';
import { ThreadApi } from '../../api/client';
import { t, type I18nKey } from '../../lib/i18n';
import { cn } from '../../lib/utils';

export type BackgroundTaskCardProps = {
  task: BackgroundTaskSnapshotWire;
  sessionId: string;
};

const STATUS_KEY: Record<BackgroundTaskSnapshotWire['status'], I18nKey> = {
  Running: 'task_running',
  Stopping: 'task_stopping',
  Completed: 'task_completed',
  Failed: 'task_failed',
  TimedOut: 'task_timed_out',
  Stopped: 'task_stopped',
  SessionEnded: 'task_session_ended',
};

const TERMINAL_COLOR = 'text-danger';
const RUNNING_COLOR = 'text-info';

export const BackgroundTaskCard = ({ task, sessionId }: BackgroundTaskCardProps) => {
  const [expanded, setExpanded] = useState(false);
  const running = task.status === 'Running' || task.status === 'Stopping';
  const statusColor = running ? RUNNING_COLOR : TERMINAL_COLOR;
  // `output_tail` is absent from snapshots with no output yet (the sender
  // skips empty tails), so the card must tolerate a missing field.
  const output = (task.output_tail ?? '').trim();

  return (
    <div className="my-2 rounded-lg border border-border bg-background px-3 py-2 text-sm">
      <div className="flex items-center gap-2">
        <span className={cn('text-xs font-medium uppercase', statusColor)}>
          {t(STATUS_KEY[task.status])}
        </span>
        <span className="min-w-0 flex-1 truncate">{task.description}</span>
        {running && (
          <button
            className="text-muted-foreground hover:text-foreground shrink-0 cursor-pointer rounded px-1.5 text-xs"
            onClick={() => new ThreadApi(sessionId).stopBackgroundTask(task.task_id)}
            title={t('task_stop')}
            type="button"
          >
            {t('task_stop')}
          </button>
        )}
      </div>
      {task.exit_code !== null && task.exit_code !== undefined && (
        <div className="text-muted-foreground mt-1 text-xs">
          exit code: {task.exit_code}
        </div>
      )}
      {output.length > 0 && (
        <button
          className="text-muted-foreground mt-1 block cursor-pointer text-xs"
          onClick={() => setExpanded((e) => !e)}
          type="button"
        >
          {expanded
            ? t('subagent_hide_activity')
            : t('task_output_lines', output.split('\n').length)}
        </button>
      )}
      {expanded && output.length > 0 && (
        <pre className="font-code mt-1 max-h-[240px] overflow-auto whitespace-pre-wrap rounded bg-muted/50 p-2 text-xs">
          {output}
        </pre>
      )}
    </div>
  );
};
