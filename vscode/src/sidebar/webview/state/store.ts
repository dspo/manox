// Webview conversation state: one pure reducer folds host messages into an
// append-only transcript plus session chrome (model, usage, turn flag), and
// a minimal store fans state changes out to the renderer.

import type { ActorEvent, ModelInfo, TokenUsageSnapshot } from '../../../protocol';
import type { HostToWebview } from '../../messages';

export interface ToolCallState {
  id: string;
  name: string;
  title: string;
  status: string;
  output: string;
  result: { output: string; isError: boolean } | null;
}

export type TranscriptItem =
  | { kind: 'user'; text: string }
  | { kind: 'assistant'; text: string }
  | { kind: 'thinking'; text: string }
  | { kind: 'tool'; call: ToolCallState }
  | {
      kind: 'approval';
      id: string;
      toolName: string;
      summary: string;
      input: unknown;
      decided: 'approved' | 'denied' | null;
    };

export interface ChatState {
  sessionId: string | null;
  cwd: string;
  turnActive: boolean;
  items: TranscriptItem[];
  models: ModelInfo[];
  currentModelId: string | null;
  usage: TokenUsageSnapshot | null;
  error: string | null;
}

export function initialState(): ChatState {
  return {
    sessionId: null,
    cwd: '',
    turnActive: false,
    items: [],
    models: [],
    currentModelId: null,
    usage: null,
    error: null,
  };
}

export function reduce(state: ChatState, msg: HostToWebview): ChatState {
  switch (msg.type) {
    case 'session_ready':
      return { ...initialState(), sessionId: msg.sessionId, cwd: msg.cwd };
    case 'session_reset':
      return initialState();
    case 'models':
      return { ...state, models: msg.models };
    case 'global_error':
      return { ...state, error: msg.message };
    case 'event':
      return reduceEvent(state, msg.event);
  }
}

function reduceEvent(state: ChatState, ev: ActorEvent): ChatState {
  switch (ev.type) {
    case 'agent_text':
      return appendStream(state, 'assistant', ev.text);
    case 'agent_thinking':
      return appendStream(state, 'thinking', ev.text);
    case 'tool_call': {
      const items = state.items.slice();
      const index = items.findIndex((i) => i.kind === 'tool' && i.call.id === ev.id);
      if (index >= 0) {
        const prev = items[index] as Extract<TranscriptItem, { kind: 'tool' }>;
        items[index] = {
          kind: 'tool',
          call: {
            ...prev.call,
            status: ev.status,
            title: ev.title || prev.call.title,
          },
        };
      } else {
        items.push({
          kind: 'tool',
          call: {
            id: ev.id,
            name: ev.name,
            title: ev.title,
            status: ev.status,
            output: '',
            result: null,
          },
        });
      }
      return { ...state, items };
    }
    case 'tool_output': {
      return mapTool(state, ev.id, (call) => ({ ...call, output: call.output + ev.chunk }));
    }
    case 'tool_result': {
      return mapTool(state, ev.id, (call) => ({
        ...call,
        result: { output: ev.output, isError: ev.is_error },
        status: ev.is_error ? 'failed' : call.status === 'running' ? 'completed' : call.status,
      }));
    }
    case 'tool_call_authorization':
      return {
        ...state,
        items: [
          ...state.items,
          {
            kind: 'approval',
            id: ev.id,
            toolName: ev.tool_name,
            summary: ev.summary,
            input: ev.input,
            decided: null,
          },
        ],
      };
    case 'turn_started':
      return { ...state, turnActive: true, error: null };
    case 'turn_finished':
      return { ...state, turnActive: false };
    case 'model_changed':
      return { ...state, currentModelId: ev.to };
    case 'current_model':
      return { ...state, currentModelId: ev.id };
    case 'usage':
      return { ...state, usage: ev.usage };
    case 'error':
      return { ...state, error: ev.message };
    default:
      return state;
  }
}

/** Fold a streamed chunk into the trailing item of its kind, else append. */
function appendStream(state: ChatState, kind: 'assistant' | 'thinking', text: string): ChatState {
  const items = state.items.slice();
  const last = items[items.length - 1];
  if (last && last.kind === kind) {
    items[items.length - 1] = { kind, text: last.text + text };
  } else {
    items.push({ kind, text });
  }
  return { ...state, items };
}

function mapTool(
  state: ChatState,
  id: string,
  f: (call: ToolCallState) => ToolCallState,
): ChatState {
  const items = state.items.slice();
  const index = items.findIndex((i) => i.kind === 'tool' && i.call.id === id);
  if (index < 0) return state;
  const item = items[index] as Extract<TranscriptItem, { kind: 'tool' }>;
  items[index] = { kind: 'tool', call: f(item.call) };
  return { ...state, items };
}

export type Listener = () => void;

export class Store {
  private state = initialState();
  private listeners = new Set<Listener>();

  get(): ChatState {
    return this.state;
  }

  dispatch(msg: HostToWebview): void {
    this.state = reduce(this.state, msg);
    this.listeners.forEach((l) => l());
  }

  /** Mark the given authorization card decided (echo of the user's click). */
  decideApproval(id: string, approved: boolean): void {
    this.state = {
      ...this.state,
      items: this.state.items.map((i) =>
        i.kind === 'approval' && i.id === id && !i.decided
          ? { ...i, decided: approved ? 'approved' : 'denied' }
          : i,
      ),
    };
    this.listeners.forEach((l) => l());
  }

  /** Optimistic echo of the user's own message before the turn starts. */
  echoUser(text: string): void {
    this.state = { ...this.state, items: [...this.state.items, { kind: 'user', text }] };
    this.listeners.forEach((l) => l());
  }

  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }
}
