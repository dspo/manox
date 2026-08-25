// Flat, reducer-friendly transcript model. Items render top-to-bottom; the
// message list regroups them into turns (a user item opens a turn, the
// assistant/thinking/tool items after it belong to that turn's reply).

import type { BackgroundTaskSnapshotWire, ToolCallStatus } from '../../../protocol';

/** UI status vocabulary (matches the tool card's label/icon tables). Wire
 * statuses fold into it; authorization states pass through. */
export type ToolUiStatus =
  | 'pending-approval'
  | 'running'
  | 'completed'
  | 'failed'
  | 'denied'
  | 'cancelled'
  | 'continued';

const PASS_THROUGH_STATUSES: ReadonlySet<ToolUiStatus> = new Set([
  'pending-approval',
  'running',
  'denied',
  'cancelled',
  'continued',
]);

/** Fold wire tool statuses into UI semantics: success → completed,
 * error → failed; everything else passes through. */
export function foldToolStatus(status: ToolCallStatus): ToolUiStatus {
  if (status === 'success') return 'completed';
  if (status === 'error') return 'failed';
  if (PASS_THROUGH_STATUSES.has(status as ToolUiStatus)) return status as ToolUiStatus;
  return 'running';
}

export interface ToolCallState {
  id: string;
  name: string;
  title: string;
  status: ToolUiStatus;
  output: string;
  isError: boolean;
  /** The autopilot reviewer allowed this call without escalation; the card
   * header renders a check-check badge ahead of the status icon. */
  autoApproved?: boolean;
}

/** Pasted attachment: live sessions carry a renderable data url, restored
 * history carries a deflated placeholder (data === null). */
export interface UserImage {
  mimeType: string;
  data: string | null;
  byteLen: number | null;
}

export type TranscriptItem =
  | {
      kind: 'user';
      id: string;
      text: string;
      /** Slash invocations render the raw invocation instead of `text`. */
      displayText?: string;
      modelId?: string | null;
      /** Unix seconds; echo submissions stamp wall-clock time. */
      timestamp?: number | null;
      images?: UserImage[];
      /** Echo id of a submission parked while a turn ran. */
      clientId?: string;
      /** Parked: the bubble shows the queued chip until the turn drains. */
      queued?: boolean;
      /** Kernel message id once the message was steered into the running
       * turn; the bubble shows the pending chip until `steer_injected`. */
      steerPendingId?: string | null;
      /** The steer was stranded by a failed/cancelled run. */
      steerFailed?: boolean;
    }
  | { kind: 'assistant'; id: string; text: string; modelId?: string | null }
  | { kind: 'thinking'; id: string; text: string }
  | { kind: 'tool'; id: string; tool: ToolCallState }
  | { kind: 'approval'; id: string; toolName: string; summary: string; input?: unknown }
  | { kind: 'compaction'; id: string; summary: string }
  | {
      kind: 'plan_review';
      id: string;
      planFile: string;
      title: string;
      content: string;
    }
  | { kind: 'background_task'; id: string; task: BackgroundTaskSnapshotWire }
  | {
      kind: 'ask_question';
      id: string;
      summary: string;
      input: unknown;
      /** Set once the user answers or cancels; the drawer hides and the
       * card morphs into the answered state fed by `tool_result`. */
      answered?: boolean;
      output?: string;
      isError?: boolean;
    };
