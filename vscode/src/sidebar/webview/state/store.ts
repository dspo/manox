// Multi-thread chat store. One fold function maps host messages into
// immutable state; per-thread states accumulate for as long as a thread is
// open, so switching views never drops in-flight events. Components observe
// via `useSyncExternalStore`; side effects (posting commands) live with the
// caller, keeping this module pure.

import type {
  ActorEvent,
  ApprovalMode,
  BackgroundTaskSnapshotWire,
  CommandEntry,
  GoalSnapshotWire,
  ModelInfo,
  ReasoningEffort,
  SubagentChildWire,
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
  /** Plan mode (read-only research) — mirrors the engine's sidecar. */
  planMode: boolean;
  items: TranscriptItem[];
  currentModelId: string | null;
  /** Model the in-flight turn started with; stamps assistant items. */
  turnModelId: string | null;
  approvalMode: ApprovalMode;
  reasoningEffort: ReasoningEffort;
  usage: TokenUsageSnapshot | null;
  cost: number;
  info: ThreadInfoSnapshot | null;
  branch: string | null;
  /** Plan review awaiting a verdict (PlanReady), for the review card. */
  pendingPlan: { planFile: string; title: string; content: string } | null;
  /** Live background-task snapshots keyed by task id. */
  backgroundTasks: Record<string, BackgroundTaskSnapshotWire>;
  /** Streamed child-session events per sub-agent (for the mini-panel). */
  subagentChildren: Record<string, SubagentChildWire[]>;
  /** Restored history still loading. */
  loading: boolean;
  /** Last error emitted for this thread; cleared when a new turn starts. */
  error: string | null;
  /** Wall-clock start of the in-flight turn; null when idle. */
  turnStartedAt: number | null;
  /** Duration of the most recent finished turn, for the meta line. */
  lastTurnDurationSec: number | null;
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
  planMode: false,
  items: [],
  currentModelId: null,
  turnModelId: null,
  // Matches the thread-side default; the actor replays the persisted effort
  // (and approval mode) on open, correcting these values for restored threads.
  reasoningEffort: 'high',
  approvalMode: 'autopilot',
  usage: null,
  cost: 0,
  info: null,
  branch: null,
  pendingPlan: null,
  backgroundTasks: {},
  subagentChildren: {},
  loading: false,
  error: null,
  turnStartedAt: null,
  lastTurnDurationSec: null,
});
const emptyInfo = (): ThreadInfoSnapshot => ({
  reasoning_effort: 'high',
  worktree_path: null,
  plan: null,
  goal: null,
  usage: {},
  cost: 0,
  pending_auth_count: 0,
  agents: [],
});

/** git_stats arrives on its own event channel, so a whole-snapshot replace
 * must keep whatever stats were already merged into the thread. The usage
 * pair rides along too: thread_info carries the same cumulative snapshot
 * the host renders, so a late push (engine materialization on restore)
 * corrects the zeroed pre-materialization `get_usage` reply. */
const mergeInfo = (t: ThreadState, info: ThreadInfoSnapshot): ThreadState => ({
  ...t,
  reasoningEffort: info.reasoning_effort,
  usage: info.usage,
  cost: info.cost,
  info: { ...info, git_stats: info.git_stats ?? t.info?.git_stats },
});

const TERMINAL_TOOL_STATUS = new Set(['completed', 'failed', 'denied', 'cancelled', 'continued']);

/** Tail window kept per tool item for streamed output. Unbounded
 * accumulation turns every chunk append into O(total) work and lets one
 * long output dominate each transcript diff; the display layer clips well
 * below this cap anyway. */
const TOOL_OUTPUT_CAP = 64_000;
const capOutputTail = (text: string): string => {
  if (text.length <= TOOL_OUTPUT_CAP) return text;
  let start = text.length - TOOL_OUTPUT_CAP;
  // A cut inside a surrogate pair would leave an orphaned half; drop the
  // pair's trailing half too so the tail stays valid UTF-16.
  const code = text.charCodeAt(start);
  if (code >= 0xdc00 && code <= 0xdfff) start += 1;
  return text.slice(start);
};

type AskQuestionTranscriptItem = Extract<TranscriptItem, { kind: 'ask_question' }>;

let echoCounter = 0;

/** Local stand-in for a steer awaiting the actor's `steer_pending`
 * confirmation; kernel message ids are UUIDs, so the literal never
 * collides. */
const STEER_PENDING_SENTINEL = 'pending';

export class Store {
  private state: ChatState = initialState;
  private readonly listeners = new Set<() => void>();
  /** Drafted sessions not yet confirmed by `session_ready`; kept outside the
   * observable state because it gates input rather than rendering. */
  private readonly creating = new Set<string>();

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  get = (): ChatState => this.state;

  dispatch(msg: HostToWebview): void {
    if (msg.type === 'events') {
      for (const ev of msg.events) {
        if (ev.type === 'session_disposed') this.creating.delete(ev.sessionId);
      }
    } else if (msg.type === 'session_ready') {
      this.creating.delete(msg.sessionId);
    } else if (msg.type === 'global_error' && this.creating.size > 0) {
      // A failed draft creation surfaces only as a global error. Release
      // every pending draft's send guard and drop the orphans; only fall
      // back to the list when the view was showing one of them.
      const stuck = [...this.creating];
      this.creating.clear();
      const perThread = { ...this.state.perThread };
      for (const id of stuck) delete perThread[id];
      const active = this.state.activeThreadId;
      this.patch({
        ...this.state,
        perThread,
        ...(active !== null && stuck.includes(active)
          ? { view: 'threads' as const, activeThreadId: null }
          : {}),
      });
    }
    this.patch(foldMessage(this.state, msg));
  }

  /** Seed a fresh thread optimistically from the home composer: switch the
   * view, echo the first message, and remember the id until `session_ready`
   * confirms the host side. */
  draftThread(sessionId: string, text: string, images?: UserImage[]): void {
    this.creating.add(sessionId);
    this.patch({ ...this.state, view: 'conversation', activeThreadId: sessionId });
    this.echoUser(sessionId, text, images);
  }

  /** Whether a drafted session is still waiting for the host's
   * `session_ready`; sending must stay blocked until it lands. */
  isCreating(sessionId: string): boolean {
    return this.creating.has(sessionId);
  }

  /** Optimistic echo of a submission; the actor never replays user
   * messages back as events. `opts.queued` marks a bubble parked while a
   * turn runs (the composer passes the flag and its echo `clientId`). */
  echoUser(
    sessionId: string,
    text: string,
    images?: UserImage[],
    opts?: { queued?: boolean; clientId?: string },
  ): void {
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
            queued: opts?.queued,
            clientId: opts?.clientId,
          },
        ],
      })),
    );
  }

  /** Local removal of a queued bubble; the caller posts `drop_queued` so
   * the actor stops parking the same message. */
  removeUser(sessionId: string, clientId: string): void {
    this.patch(
      updateThread(this.state, sessionId, (t) => ({
        ...t,
        items: t.items.filter(
          (i) => !(i.kind === 'user' && i.clientId === clientId),
        ),
      })),
    );
  }

  /** Optimistic flip of a queued bubble into the pending-steer state, so a
   * double-click cannot enqueue the steer twice; the actor's
   * `steer_pending` replaces the sentinel with the kernel message id. */
  markSteerPending(sessionId: string, clientId: string): void {
    this.patch(
      updateThread(this.state, sessionId, (t) => ({
        ...t,
        items: t.items.map((i) =>
          i.kind === 'user' && i.clientId === clientId
            ? { ...i, steerPendingId: STEER_PENDING_SENTINEL }
            : i,
        ),
      })),
    );
  }

  /** Drop a pending approval card from the transcript; the caller posts the
   * actual verdict to the host. (AskUserQuestion cards morph via
   * `respondAsk` instead of being dropped.) */
  decideApproval(sessionId: string, id: string): void {
    this.patch(
      updateThread(this.state, sessionId, (t) => ({
        ...t,
        items: t.items.filter(
          (i) => !((i.kind === 'approval' || i.kind === 'ask_question') && i.id === id),
        ),
      })),
    );
  }

  /** Mark an `AskUserQuestion` card answered (submit or cancel path); the
   * caller posts the actual verdict. The card stays and morphs into the
   * answered state fed by the completion events, mirroring the native
   * drawer — dropping it would lose the human title and strand the result
   * in a code-named tool item. */
  respondAsk(sessionId: string, id: string): void {
    this.patch(
      updateThread(this.state, sessionId, (t) => ({
        ...t,
        items: t.items.map((i) =>
          i.kind === 'ask_question' && i.id === id ? { ...i, answered: true } : i,
        ),
      })),
    );
  }

  /** Consume a plan review card (any verdict path) and clear the pending
   * review state. */
  clearPlanReview(sessionId: string): void {
    this.patch(
      updateThread(this.state, sessionId, (t) => ({
        ...t,
        pendingPlan: null,
        items: t.items.filter((i) => i.kind !== 'plan_review'),
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
    // `running` mirrors the same ThreadEvents that drive `turn_started` /
    // `turn_finished`, and the store mutation precedes the event emission,
    // so syncing from it keeps the stop button correct even when the event
    // stream was missed (webview reload while a turn was in flight).
    if (t && (t.title !== item.title || t.turnActive !== item.running)) {
      perThread = { ...perThread, [item.id]: { ...t, title: item.title, turnActive: item.running } };
    }
  }
  return { ...state, threads, perThread };
}

function foldMessage(state: ChatState, msg: HostToWebview): ChatState {
  switch (msg.type) {
    case 'session_ready': {
      // Merge-safe: an optimistic draft (or a live thread being re-announced)
      // keeps its accumulated items, and only the session's origin metadata
      // changes. A restored replay still replaces the items wholesale when
      // its thread_history snapshot lands. A restored thread that is still
      // running never re-emits `turn_started`, so seed `turnActive` from the
      // thread list's `running` flag (reload recovery).
      const existing = state.perThread[msg.sessionId];
      const row = state.threads.find((x) => x.id === msg.sessionId);
      const running =
        msg.kind === 'restored'
          ? (row?.running ?? existing?.turnActive ?? false)
          : (existing?.turnActive ?? false);
      const thread = existing
        ? { ...existing, cwd: msg.cwd, loading: msg.kind === 'restored', turnActive: running }
        : { ...initThread(msg.sessionId, msg.cwd), loading: msg.kind === 'restored', turnActive: running };
      return {
        ...state,
        view: 'conversation',
        activeThreadId: msg.sessionId,
        error: null,
        perThread: { ...state.perThread, [msg.sessionId]: thread },
      };
    }
    case 'models':
      return { ...state, models: msg.models };
    case 'threads':
      return foldThreads(state, msg.threads);
    case 'commands':
      return { ...state, commands: msg.commands };
    case 'thread_info':
      return updateThread(state, msg.sessionId, (t) => mergeInfo(t, msg.info));
    case 'global_error':
      return { ...state, error: msg.message };
    case 'open_turn_navigator':
      // Host-requested navigator toggle (macOS cmd+m); the component
      // subscribes via `onOpenTurnNavigator`, the store is untouched.
      return state;
    // One fold pass per coalesced frame: the patch (and therefore the
    // listener notification) happens once for the whole batch instead of
    // once per streamed event.
    case 'events':
      return msg.events.reduce((s, ev) => foldEvent(s, ev), state);
  }
}


function foldEvent(state: ChatState, ev: ActorEvent): ChatState {
  // Session-scoped errors stay with their thread so a background failure
  // never surfaces in another conversation's banner.
  if (ev.type === 'error') {
    if (typeof ev.sessionId === 'string') {
      // Sessions without local state would materialize a ghost thread via
      // updateThread's init fallback; their errors are dropped instead.
      if (!state.perThread[ev.sessionId]) return state;
      return updateThread(state, ev.sessionId, (t) => ({ ...t, error: ev.message }));
    }
    return { ...state, error: ev.message };
  }
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
    case 'turn_started': {
      // A fresh turn supersedes any stale error from the previous one;
      // queued bubbles entered this round (the actor drained them into it).
      return {
        ...t,
        turnActive: true,
        turnModelId: t.currentModelId,
        turnStartedAt: Date.now(),
        error: null,
        items: t.items.map((i) =>
          i.kind === 'user' && i.queued ? { ...i, queued: false } : i,
        ),
      };
    }
    case 'turn_finished': {
      // Steers stranded by a failed/cancelled run become failed bubbles;
      // pending ids the actor never listed are a race — clear them anyway.
      const stranded = new Set(ev.strandedSteerIds ?? []);
      return {
        ...t,
        turnActive: false,
        lastTurnDurationSec:
          t.turnStartedAt === null
            ? t.lastTurnDurationSec
            : Math.max(0, Math.round((Date.now() - t.turnStartedAt) / 1000)),
        turnStartedAt: null,
        // A successful finish supersedes a stale mid-turn error: the loop can
        // recover (auto-retry, compact-and-retry) without a `turn_started` in
        // between, so the banner and captain icon must follow the turn's real
        // outcome. A failed finish keeps the error.
        error: ev.failed ? t.error : null,
        items: t.items.map((i) => {
          if (i.kind !== 'user' || !i.steerPendingId) return i;
          return stranded.has(i.steerPendingId)
            ? { ...i, steerPendingId: null, steerFailed: true }
            : { ...i, steerPendingId: null };
        }),
      };
    }
    case 'stop':
      return {
        ...t,
        turnActive: false,
        lastTurnDurationSec:
          t.turnStartedAt === null
            ? t.lastTurnDurationSec
            : Math.max(0, Math.round((Date.now() - t.turnStartedAt) / 1000)),
        turnStartedAt: null,
      };
    case 'steer_pending':
      return {
        ...t,
        items: t.items.map((i) =>
          i.kind === 'user' && i.clientId === ev.clientId
            ? { ...i, steerPendingId: ev.messageId }
            : i,
        ),
      };
    case 'steer_injected':
      return {
        ...t,
        items: t.items.map((i) =>
          i.kind === 'user' && i.steerPendingId === ev.messageId
            ? { ...i, steerPendingId: null }
            : i,
        ),
      };
    case 'agent_text':
      return appendAssistantText(t, ev.text);
    case 'agent_thinking':
      return appendThinkingText(t, ev.text);
    case 'tool_call': {
      if (ev.name === 'AskUserQuestion') {
        const askIdx = t.items.findIndex((i) => i.kind === 'ask_question' && i.id === ev.id);
        // The kernel emits the start (running) and the tool emits the
        // pending-approval marker before the authorization card exists; both
        // would spawn a generic tool item beside the card. The card is the
        // single surface for the whole lifecycle, so every AskUserQuestion
        // event without a card to morph is dropped.
        if (askIdx === -1) return t;
        // Only terminal statuses morph the card into the answered state;
        // running/pending markers keep the interactive drawer live.
        const status = foldToolStatus(ev.status);
        if (!TERMINAL_TOOL_STATUS.has(status)) return t;
        const items = t.items.slice();
        items[askIdx] = { ...(items[askIdx] as AskQuestionTranscriptItem), answered: true };
        return { ...t, items };
      }
      // An AutoPilot escalation re-brands the real tool's card as
      // AskUserQuestion under the same id; the gate's restore event hands
      // the id back to the real tool (name != AskUserQuestion). Drop the
      // question card so the real tool item owns the completion and result.
      let base = t;
      const askIdx = t.items.findIndex((i) => i.kind === 'ask_question' && i.id === ev.id);
      if (askIdx !== -1) {
        const items = t.items.slice();
        items.splice(askIdx, 1);
        base = { ...t, items };
      }
      return upsertToolItem(base, ev.id, (prev) => {
        const status = foldToolStatus(ev.status);
        return {
          id: ev.id,
          name: ev.name,
          // The engine replays `title: tool_name` on the completion call;
          // keep the human title (command line, path, question label) that
          // the start call carried instead of degrading to the code name.
          title: prev?.title && ev.title === ev.name ? prev.title : (ev.title || prev?.title || ev.name),
          status,
          output: prev?.output ?? '',
          isError: status === 'failed' ? true : (prev?.isError ?? false),
        };
      });
    }
    case 'tool_output':
      return upsertToolItem(t, ev.id, (prev) => ({
        id: ev.id,
        name: prev?.name ?? '',
        title: prev?.title ?? ev.id,
        status: prev?.status ?? 'running',
        output: capOutputTail((prev?.output ?? '') + ev.chunk),
        isError: prev?.isError ?? false,
      }));
    case 'tool_result':
      // An answered AskUserQuestion card absorbs its own result in place
      // (the morph keeps the card's human title); everything else feeds the
      // ordinary tool item.
      const askIdx = t.items.findIndex((i) => i.kind === 'ask_question' && i.id === ev.id);
      if (askIdx !== -1) {
        const items = t.items.slice();
        items[askIdx] = {
          ...(items[askIdx] as AskQuestionTranscriptItem),
          answered: true,
          output: ev.output,
          isError: ev.is_error,
        };
        return { ...t, items };
      }
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
        output: capOutputTail(ev.output),
        isError: ev.is_error,
      }));
    case 'tool_call_authorization':
      return {
        ...t,
        items: [
          ...t.items,
          ev.tool_name === 'AskUserQuestion'
            ? { kind: 'ask_question', id: ev.id, summary: ev.summary, input: ev.input }
            : {
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
    case 'reasoning_effort_changed':
      return { ...t, reasoningEffort: ev.effort };
    case 'approval_mode_changed':
      return { ...t, approvalMode: ev.mode };
    case 'current_model':
      return { ...t, currentModelId: ev.id };
    case 'usage':
      return { ...t, usage: ev.usage, cost: ev.cost };
    case 'thread_history':
      return { ...t, items: wireMessagesToTranscriptItems(ev.messages), loading: false };
    case 'thread_info':
      return mergeInfo(t, ev.info);
    case 'branch':
      return { ...t, branch: ev.branch };
    case 'git_stats':
      return { ...t, info: { ...(t.info ?? emptyInfo()), git_stats: ev.stats } };
    case 'history_progress':
      return { ...t, loading: true };
    case 'plan_mode_changed':
      return { ...t, planMode: ev.enabled };
    case 'plan_updated':
      return { ...t, info: { ...(t.info ?? emptyInfo()), plan: ev.snapshot } };
    case 'goal_changed':
      return { ...t, info: { ...(t.info ?? emptyInfo()), goal: ev.snapshot } };
    case 'worktree_changed':
      return {
        ...t,
        info: { ...(t.info ?? emptyInfo()), worktree_path: ev.active ? ev.path : null },
      };
    case 'compaction':
      return {
        ...t,
        items: [
          ...t.items,
          { kind: 'compaction', id: `compaction-${t.items.length}`, summary: ev.summary },
        ],
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
      // The pi backend emits no SubagentStarted, so the first progress
      // sighting creates the row (upsert).
      const exists = info.agents.some((a) => a.id === ev.id);
      const updated = exists
        ? info.agents.map((a) =>
            a.id === ev.id
              ? {
                  ...a,
                  tool_uses: ev.tool_uses,
                  latest_activity: ev.latest_activity,
                  status: ev.status,
                }
              : a,
          )
        : [
            ...info.agents,
            {
              id: ev.id,
              agent_type: ev.agent_type,
              description: '',
              tool_uses: ev.tool_uses,
              latest_activity: ev.latest_activity,
              status: ev.status,
            },
          ];
      return { ...t, info: { ...info, agents: updated } };
    }
    case 'plan_ready':
      return {
        ...t,
        pendingPlan: {
          planFile: ev.plan_file,
          title: ev.title,
          content: ev.content ?? '',
        },
        items: [
          ...t.items,
          {
            kind: 'plan_review',
            id: `plan-review-${t.items.length}`,
            planFile: ev.plan_file,
            title: ev.title,
            content: ev.content ?? '',
          },
        ],
      };
    case 'background_task_updated': {
      const task = ev.snapshot;
      // The card is appended once; later snapshots only touch the map and the
      // render layer reads the live snapshot from it — streaming updates stay
      // O(1) instead of scanning the transcript per snapshot.
      const known = task.task_id in t.backgroundTasks;
      const backgroundTasks = { ...t.backgroundTasks, [task.task_id]: task };
      const items: TranscriptItem[] = known
        ? t.items
        : [...t.items, { kind: 'background_task', id: `bg-${task.task_id}`, task }];
      return { ...t, backgroundTasks, items };
    }
    case 'subagent_child': {
      const prior = t.subagentChildren[ev.id] ?? [];
      const next = [...prior, ev.event].slice(-200);
      return { ...t, subagentChildren: { ...t.subagentChildren, [ev.id]: next } };
    }
    // Covered elsewhere or not surfaced: session_created (host handshake),
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
          output: capOutputTail(result.content),
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
