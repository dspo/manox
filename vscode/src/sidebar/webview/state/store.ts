// Multi-thread chat store. One fold function maps host messages into
// immutable state; per-thread states accumulate for as long as a thread is
// open, so switching views never drops in-flight events. Components observe
// via `useSyncExternalStore`; side effects (posting commands) live with the
// caller, keeping this module pure.

import type {
  ActorEvent,
  ApprovalMode,
  CommandEntry,
  ModelInfo,
  ThreadInfoSnapshot,
  ThreadListItem,
  TokenUsageSnapshot,
  WireMessage,
} from '../../../protocol';
import { isSessionEvent } from '../../../protocol';
import type { HostToWebview } from '../../messages';
import type { ToolCallState, TranscriptItem, UserImage } from './transcript';
import { foldToolStatus } from './transcript';

export type { ToolCallState, ToolUiStatus, TranscriptItem, UserImage } from './transcript';

export interface ThreadState {
  sessionId: string;
  cwd: string;
  /** Display title from the thread registry; fallback for brand-new
   * threads before the agent names them. */
  title: string;
  turnActive: boolean;
  items: TranscriptItem[];
  currentModelId: string | null;
  /** Model the in-flight turn started with; stamps assistant items. */
  turnModelId: string | null;
  approvalMode: ApprovalMode;
  usage: TokenUsageSnapshot | null;
  cost: number;
  info: ThreadInfoSnapshot | null;
  branch: string | null;
  /** Restored history still loading. */
  loading: boolean;
}

export interface ChatState {
  view: 'threads' | 'conversation';
  threads: ThreadListItem[];
  activeThreadId: string | null;
  perThread: Record<string, ThreadState>;
  models: ModelInfo[];
  commands: CommandEntry[];
  error: string | null;
}

const initialState: ChatState = {
  view: 'threads',
  threads: [],
  activeThreadId: null,
  perThread: {},
  models: [],
  commands: [],
  error: null,
};

const initThread = (sessionId: string, cwd: string): ThreadState => ({
  sessionId,
  cwd,
  title: 'New conversation',
  turnActive: false,
  items: [],
  currentModelId: null,
  turnModelId: null,
  approvalMode: 'danger',
  usage: null,
  cost: 0,
  info: null,
  branch: null,
  loading: false,
});

const emptyInfo = (): ThreadInfoSnapshot => ({
  worktree_path: null,
  plan: null,
  usage: {},
  cost: 0,
  pending_auth_count: 0,
  agents: [],
});

const TERMINAL_TOOL_STATUS = new Set(['completed', 'failed', 'denied', 'cancelled', 'continued']);

let echoCounter = 0;

export class Store {
  private state: ChatState = initialState;
  private readonly listeners = new Set<() => void>();

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  get = (): ChatState => this.state;

  dispatch(msg: HostToWebview): void {
    this.patch(foldMessage(this.state, msg));
  }

  /** Optimistic echo of a submission; the actor never replays user
   * messages back as events. */
  echoUser(sessionId: string, text: string, images?: UserImage[]): void {
    this.patch(
      updateThread(this.state, sessionId, (t) => ({
        ...t,
        items: [
          ...t.items,
          {
            kind: 'user',
            id: `echo-${++echoCounter}`,
            text,
            modelId: t.currentModelId,
            timestamp: Date.now() / 1000,
            images: images?.length ? images : undefined,
          },
        ],
      })),
    );
  }

  /** Drop an authorization prompt from the transcript; the caller posts the
   * actual decision to the host. */
  decideApproval(sessionId: string, id: string): void {
    this.patch(
      updateThread(this.state, sessionId, (t) => ({
        ...t,
        items: t.items.filter((i) => !(i.kind === 'approval' && i.id === id)),
      })),
    );
  }

  /** Switch to a thread that already has local state. Threads without local
   * state are opened through the host instead (session_ready performs the
   * switch). */
  openLocal(sessionId: string): void {
    this.patch({ ...this.state, view: 'conversation', activeThreadId: sessionId });
  }

  backToList(): void {
    this.patch({ ...this.state, view: 'threads' });
  }

  private patch(next: ChatState): void {
    if (next === this.state) return;
    this.state = next;
    for (const listener of this.listeners) listener();
  }
}

function updateThread(
  state: ChatState,
  sessionId: string,
  f: (t: ThreadState) => ThreadState,
): ChatState {
  const current = state.perThread[sessionId] ?? initThread(sessionId, '');
  const next = f(current);
  if (next === current && state.perThread[sessionId]) return state;
  return { ...state, perThread: { ...state.perThread, [sessionId]: next } };
}

function foldThreads(state: ChatState, threads: ThreadListItem[]): ChatState {
  let perThread = state.perThread;
  for (const item of threads) {
    const t = perThread[item.id];
    if (t && t.title !== item.title) {
      perThread = { ...perThread, [item.id]: { ...t, title: item.title } };
    }
  }
  return { ...state, threads, perThread };
}

function foldMessage(state: ChatState, msg: HostToWebview): ChatState {
  switch (msg.type) {
    case 'session_ready':
      return {
        ...state,
        view: 'conversation',
        activeThreadId: msg.sessionId,
        error: null,
        perThread: {
          ...state.perThread,
          [msg.sessionId]: {
            ...initThread(msg.sessionId, msg.cwd),
            loading: msg.kind === 'restored',
          },
        },
      };
    case 'models':
      return { ...state, models: msg.models };
    case 'threads':
      return foldThreads(state, msg.threads);
    case 'commands':
      return { ...state, commands: msg.commands };
    case 'thread_info':
      return updateThread(state, msg.sessionId, (t) => ({ ...t, info: msg.info }));
    case 'global_error':
      return { ...state, error: msg.message };
    case 'event':
      return foldEvent(state, msg.event);
  }
}

function foldEvent(state: ChatState, ev: ActorEvent): ChatState {
  if (ev.type === 'error') return { ...state, error: ev.message };
  if (!isSessionEvent(ev)) {
    switch (ev.type) {
      case 'models':
        return { ...state, models: ev.models };
      case 'threads_updated':
        return foldThreads(state, ev.threads);
      case 'commands':
        return { ...state, commands: ev.commands };
      default:
        return state;
    }
  }
  if (ev.type === 'session_disposed') {
    const perThread = { ...state.perThread };
    delete perThread[ev.sessionId];
    const wasActive = state.activeThreadId === ev.sessionId;
    return {
      ...state,
      perThread,
      activeThreadId: wasActive ? null : state.activeThreadId,
      view: wasActive ? 'threads' : state.view,
    };
  }
  return updateThread(state, ev.sessionId, (t) => foldThreadEvent(t, ev));
}

function foldThreadEvent(t: ThreadState, ev: ActorEvent & { sessionId: string }): ThreadState {
  switch (ev.type) {
    case 'turn_started':
      return { ...t, turnActive: true, turnModelId: t.currentModelId };
    case 'turn_finished':
    case 'stop':
      return { ...t, turnActive: false };
    case 'agent_text':
      return appendAssistantText(t, ev.text);
    case 'agent_thinking':
      return appendThinkingText(t, ev.text);
    case 'tool_call':
      return upsertToolItem(t, ev.id, (prev) => {
        const status = foldToolStatus(ev.status);
        return {
          id: ev.id,
          name: ev.name,
          title: ev.title || prev?.title || ev.name,
          status,
          output: prev?.output ?? '',
          isError: status === 'failed' ? true : (prev?.isError ?? false),
        };
      });
    case 'tool_output':
      return upsertToolItem(t, ev.id, (prev) => ({
        id: ev.id,
        name: prev?.name ?? '',
        title: prev?.title ?? ev.id,
        status: prev?.status ?? 'running',
        output: (prev?.output ?? '') + ev.chunk,
        isError: prev?.isError ?? false,
      }));
    case 'tool_result':
      return upsertToolItem(t, ev.id, (prev) => ({
        id: ev.id,
        name: prev?.name ?? '',
        title: prev?.title ?? ev.id,
        // The terminal tool_call already folds to its final status; this
        // covers results racing ahead of their status update.
        status: ev.is_error
          ? 'failed'
          : prev && TERMINAL_TOOL_STATUS.has(prev.status)
            ? prev.status
            : 'completed',
        output: ev.output,
        isError: ev.is_error,
      }));
    case 'tool_call_authorization':
      return {
        ...t,
        items: [
          ...t.items,
          {
            kind: 'approval',
            id: ev.id,
            toolName: ev.tool_name,
            summary: ev.summary,
            input: ev.input,
          },
        ],
      };
    case 'model_changed':
      return { ...t, currentModelId: ev.to };
    case 'approval_mode_changed':
      return { ...t, approvalMode: ev.mode };
    case 'current_model':
      return { ...t, currentModelId: ev.id };
    case 'usage':
      return { ...t, usage: ev.usage, cost: ev.cost };
    case 'thread_history':
      return { ...t, items: wireMessagesToTranscriptItems(ev.messages), loading: false };
    case 'thread_info':
      return { ...t, info: ev.info };
    case 'branch':
      return { ...t, branch: ev.branch };
    case 'history_progress':
      return { ...t, loading: true };
    case 'plan_updated':
      return { ...t, info: { ...(t.info ?? emptyInfo()), plan: ev.snapshot } };
    case 'worktree_changed':
      return {
        ...t,
        info: { ...(t.info ?? emptyInfo()), worktree_path: ev.active ? ev.path : null },
      };
    case 'subagent_started': {
      const info = t.info ?? emptyInfo();
      if (info.agents.some((a) => a.id === ev.id)) return t;
      return {
        ...t,
        info: {
          ...info,
          agents: [
            ...info.agents,
            {
              id: ev.id,
              agent_type: ev.agent_type,
              description: ev.description,
              tool_uses: 0,
              latest_activity: null,
              status: 'running',
            },
          ],
        },
      };
    }
    case 'subagent_progress': {
      const info = t.info ?? emptyInfo();
      return {
        ...t,
        info: {
          ...info,
          agents: info.agents.map((a) =>
            a.id === ev.id
              ? {
                  ...a,
                  tool_uses: ev.tool_uses,
                  latest_activity: ev.latest_activity,
                  status: ev.status,
                }
              : a,
          ),
        },
      };
    }
    // Covered elsewhere or not surfaced: session_created (host handshake),
    // plan_ready / plan_mode_changed (the snapshot arrives via plan_updated),
    // token_usage (fine-grained), models/threads_updated/commands (global).
    default:
      return t;
  }
}

function appendAssistantText(t: ThreadState, text: string): ThreadState {
  const last = t.items[t.items.length - 1];
  if (last && last.kind === 'assistant') {
    return {
      ...t,
      items: [...t.items.slice(0, -1), { ...last, text: last.text + text }],
    };
  }
  return {
    ...t,
    items: [
      ...t.items,
      {
        kind: 'assistant',
        id: `assistant-${t.items.length}`,
        text,
        modelId: t.turnModelId ?? t.currentModelId,
      },
    ],
  };
}

function appendThinkingText(t: ThreadState, text: string): ThreadState {
  const last = t.items[t.items.length - 1];
  if (last && last.kind === 'thinking') {
    return {
      ...t,
      items: [...t.items.slice(0, -1), { ...last, text: last.text + text }],
    };
  }
  return {
    ...t,
    items: [...t.items, { kind: 'thinking', id: `thinking-${t.items.length}`, text }],
  };
}

function upsertToolItem(
  t: ThreadState,
  id: string,
  f: (prev: ToolCallState | undefined) => ToolCallState,
): ThreadState {
  const index = t.items.findIndex((i) => i.kind === 'tool' && i.id === id);
  if (index === -1) {
    return { ...t, items: [...t.items, { kind: 'tool', id, tool: f(undefined) }] };
  }
  const item = t.items[index];
  if (item.kind !== 'tool') return t;
  const items = t.items.slice();
  items[index] = { kind: 'tool', id, tool: f(item.tool) };
  return { ...t, items };
}

/** Pure mapping from restored wire messages to transcript items. ToolUse
 * blocks open a tool item; a later ToolResult with a matching id replaces
 * it. Images arrive as deflated placeholders (data stripped on the wire). */
export function wireMessagesToTranscriptItems(messages: WireMessage[]): TranscriptItem[] {
  const items: TranscriptItem[] = [];
  const toolNames = new Map<string, string>();
  for (const msg of messages) {
    const ui = msg.ui ?? {};
    if (msg.role === 'user') {
      if (msg.provenance === 'goal_continuation' || msg.provenance === 'goal_objective_update') {
        continue;
      }
      let text = '';
      const images: UserImage[] = [];
      for (const block of msg.content) {
        if ('Text' in block) text += block.Text;
        if ('Image' in block) {
          images.push({
            mimeType: block.Image.mime_type,
            data: null,
            byteLen: block.Image.byte_len,
          });
        }
      }
      if (!text && images.length === 0) continue;
      items.push({
        kind: 'user',
        id: msg.id,
        text,
        displayText: ui.display_text ?? undefined,
        modelId: ui.model_id ?? null,
        timestamp: msg.timestamp,
        images: images.length ? images : undefined,
      });
      continue;
    }
    if (msg.role === 'assistant') {
      for (const block of msg.content) {
        if ('Text' in block) {
          if (block.Text.trim()) {
            items.push({
              kind: 'assistant',
              id: `${msg.id}-${items.length}`,
              text: block.Text,
              modelId: ui.model_id ?? null,
            });
          }
        } else if ('Thinking' in block) {
          if (block.Thinking.text.trim()) {
            items.push({
              kind: 'thinking',
              id: `${msg.id}-${items.length}`,
              text: block.Thinking.text,
            });
          }
        } else if ('ToolUse' in block) {
          toolNames.set(block.ToolUse.id, block.ToolUse.name);
          items.push({
            kind: 'tool',
            id: block.ToolUse.id,
            tool: {
              id: block.ToolUse.id,
              name: block.ToolUse.name,
              title: `${block.ToolUse.name}(${block.ToolUse.raw_input})`,
              status: 'completed',
              output: '',
              isError: false,
            },
          });
        } else if ('Compaction' in block) {
          if (block.Compaction.trim()) {
            items.push({
              kind: 'compaction',
              id: `${msg.id}-${items.length}`,
              summary: block.Compaction,
            });
          }
        }
      }
      continue;
    }
    if (msg.provenance === 'tool') {
      for (const block of msg.content) {
        if (!('ToolResult' in block)) continue;
        const result = block.ToolResult;
        const name = result.tool_name || toolNames.get(result.tool_use_id) || 'tool';
        const existing = items.findIndex((i) => i.kind === 'tool' && i.id === result.tool_use_id);
        const tool: ToolCallState = {
          id: result.tool_use_id,
          name,
          title:
            existing >= 0 ? (items[existing] as { kind: 'tool'; tool: ToolCallState }).tool.title : name,
          status: result.is_error ? 'failed' : 'completed',
          output: result.content,
          isError: result.is_error,
        };
        if (existing >= 0) {
          items[existing] = { kind: 'tool', id: result.tool_use_id, tool };
        } else {
          items.push({ kind: 'tool', id: result.tool_use_id, tool });
        }
      }
    }
  }
  return items;
}
