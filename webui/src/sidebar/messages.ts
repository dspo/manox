// postMessage protocol between the sidebar provider (host) and the webview
// renderer. Actor payloads cross the boundary verbatim inside `event`,
// except the global snapshots unwrapped into their own messages. Per-thread
// messages always carry their sessionId — view switching is pure webview
// state, so the host routes by id and never infers a "current" session.

import type {
  ActorEvent,
  ApprovalMode,
  CommandEntry,
  GoalAction,
  ImageAttachment,
  ModelInfo,
  PlanVerdictChoice,
  ReasoningEffort,
  ThreadInfoSnapshot,
  ThreadListItem,
} from '../protocol';

export type WebviewToHost =
  | {
      type: 'submit';
      sessionId: string;
      text: string;
      images?: ImageAttachment[];
      /** Echo identifier for the queued-bubble lifecycle (`steer` /
       * `drop_queued` target the same id the host relayed back). */
      clientId?: string;
    }
  | { type: 'steer'; sessionId: string; clientId: string; text: string; images?: ImageAttachment[] }
  | { type: 'drop_queued'; sessionId: string; clientId: string }
  | { type: 'approve'; sessionId: string; id: string; allow: boolean }
  | { type: 'cancel'; sessionId: string }
  | { type: 'set_model'; sessionId: string; id: string }
  | { type: 'set_reasoning_effort'; sessionId: string; effort: ReasoningEffort }
  | { type: 'set_approval_mode'; sessionId: string; mode: ApprovalMode }
  | { type: 'set_plan_mode'; sessionId: string; enabled: boolean }
  | { type: 'plan_verdict'; sessionId: string; choice: PlanVerdictChoice }
  | { type: 'plan_execute_fresh'; sessionId: string; planFile: string; cwd: string }
  | { type: 'goal'; sessionId: string; action: GoalAction; objective?: string; budget?: number }
  | { type: 'stop_background_task'; sessionId: string; taskId: string }
  | {
      type: 'answer_question';
      sessionId: string;
      id: string;
      answers: [string, string][];
      response: string | null;
    }
  | { type: 'request_models' }
  | { type: 'request_usage'; sessionId: string }
  | { type: 'request_thread_info'; sessionId: string }
  // Optional fields carry the home-composer first message: the webview
  // generates the id so it can render the draft before the session exists.
  | {
      type: 'new_session';
      sessionId?: string;
      text?: string;
      images?: ImageAttachment[];
      modelId?: string;
    }
  | { type: 'archive_thread'; sessionId: string; archived: boolean }
  | { type: 'pin_thread'; sessionId: string; pinned: boolean }
  | { type: 'list_threads' }
  | { type: 'open_thread'; sessionId: string }
  | { type: 'focus_thread'; sessionId?: string }
  | { type: 'list_commands' };

export type HostToWebview =
  | { type: 'session_ready'; sessionId: string; cwd: string; kind: 'fresh' | 'restored' }
  // Coalesced session events: the host drains its per-frame buffer as one
  // message, so a streaming flood crosses the bridge once per frame instead
  // of once per event. Order inside the array is arrival order.
  | { type: 'events'; events: ActorEvent[] }
  | { type: 'models'; models: ModelInfo[] }
  | { type: 'threads'; threads: ThreadListItem[] }
  | { type: 'commands'; commands: CommandEntry[] }
  | { type: 'thread_info'; sessionId: string; info: ThreadInfoSnapshot }
  | { type: 'global_error'; message: string }
  /** macOS cmd+m lands here via the manox.openTurnNavigator command (the
   * OS minimize accelerator would otherwise swallow the key before the
   * webview DOM sees it); the webview toggles the turn navigator. */
  | { type: 'open_turn_navigator' };
