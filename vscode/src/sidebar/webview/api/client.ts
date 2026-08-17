// Typed bridge to the extension host: postMessage out, typed HostToWebview
// payloads in. Every per-thread call carries its sessionId so the store can
// address any live thread; view switching stays inside the webview and never
// crosses this boundary.

import type {
  ApprovalMode,
  GoalAction,
  ImageAttachment,
  PlanVerdictChoice,
} from '../../../protocol';
import type { HostToWebview, WebviewToHost } from '../../messages';

interface HostApi {
  postMessage(msg: WebviewToHost): void;
}

declare function acquireVsCodeApi(): HostApi;

const host = acquireVsCodeApi();

const listeners = new Set<(message: HostToWebview) => void>();

window.addEventListener('message', (raw: MessageEvent) => {
  const message = raw.data as HostToWebview;
  for (const listener of listeners) listener(message);
});

/** Subscribe to host messages; returns an unsubscribe function. */
export function onHostMessage(listener: (message: HostToWebview) => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function post(message: WebviewToHost): void {
  host.postMessage(message);
}

/** Per-thread command surface; one instance per live thread id. */
export class ThreadApi {
  constructor(readonly sessionId: string) {}

  submit(text: string, images?: ImageAttachment[]): void {
    post({ type: 'submit', sessionId: this.sessionId, text, images });
  }

  approve(id: string, allow: boolean): void {
    post({ type: 'approve', sessionId: this.sessionId, id, allow });
  }

  /** Resolve an `AskUserQuestion` card: per-question selections (labels
   * joined by ", ") plus an optional free-form note that overrides them. */
  answerQuestion(id: string, answers: [string, string][], response: string | null): void {
    post({ type: 'answer_question', sessionId: this.sessionId, id, answers, response });
  }

  cancel(): void {
    post({ type: 'cancel', sessionId: this.sessionId });
  }

  setModel(id: string): void {
    post({ type: 'set_model', sessionId: this.sessionId, id });
  }

  setApprovalMode(mode: ApprovalMode): void {
    post({ type: 'set_approval_mode', sessionId: this.sessionId, mode });
  }

  setPlanMode(enabled: boolean): void {
    post({ type: 'set_plan_mode', sessionId: this.sessionId, enabled });
  }

  planVerdict(choice: PlanVerdictChoice): void {
    post({ type: 'plan_verdict', sessionId: this.sessionId, choice });
  }

  /** Execute-fresh: the host archives this session and seeds a new one with
   * the plan (host-side orchestration via `plan_execute_fresh`). */
  planExecuteFresh(planFile: string, cwd: string): void {
    post({ type: 'plan_execute_fresh', sessionId: this.sessionId, planFile, cwd });
  }

  goal(action: GoalAction, objective?: string, budget?: number): void {
    post({ type: 'goal', sessionId: this.sessionId, action, objective, budget });
  }

  stopBackgroundTask(taskId: string): void {
    post({ type: 'stop_background_task', sessionId: this.sessionId, taskId });
  }

  requestUsage(): void {
    post({ type: 'request_usage', sessionId: this.sessionId });
  }

  requestThreadInfo(): void {
    post({ type: 'request_thread_info', sessionId: this.sessionId });
  }

  focus(): void {
    post({ type: 'focus_thread', sessionId: this.sessionId });
  }
}

/** Global command surface (thread registry, models, slash entries). */
export const api = {
  requestModels(): void {
    post({ type: 'request_models' });
  },
  /** Optional payload = home-composer first message: the caller picks the id
   * so an optimistic draft can render before the session exists. */
  newSession(opts: {
    sessionId?: string;
    text?: string;
    images?: ImageAttachment[];
    modelId?: string;
  }): void {
    post({ type: 'new_session', ...opts });
  },
  listThreads(): void {
    post({ type: 'list_threads' });
  },
  archiveThread(sessionId: string, archived: boolean): void {
    post({ type: 'archive_thread', sessionId, archived });
  },
  pinThread(sessionId: string, pinned: boolean): void {
    post({ type: 'pin_thread', sessionId, pinned });
  },
  openThread(sessionId: string): void {
    post({ type: 'open_thread', sessionId });
  },
  /** Clear the focused thread (leaving the conversation view) so turns that
   * finish afterwards mark it unread. */
  blurThread(): void {
    post({ type: 'focus_thread' });
  },
  listCommands(): void {
    post({ type: 'list_commands' });
  },
};
