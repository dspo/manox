// Multi-thread chat store. One fold function maps `FromServer` messages into
// immutable state; per-thread states accumulate for as long as a thread is
// open, so switching views never drops in-flight events. Components observe
// via `useSyncExternalStore`; side effects (posting `FromClient`) live with
// the caller, keeping this module pure.
//
// The store is driven entirely by push delivery: `ServerNote` notifications
// fold into state, `ServerCall` requests render adjudication cards, and
// `Response` frames are dropped by the bridge (list-type calls reuse their
// matching `ThreadsUpdated` / `Models` / `Commands` notification). Session
// readiness is derived from `SessionCreated` (fresh) and `ThreadHistory`
// (restored) — there is no host-pushed `session_ready`.

import type {
	ApprovalMode,
	BackgroundTaskSnapshotWire,
	CommandEntry,
	FromServer,
	GoalSnapshotWire,
	ModelInfo,
	ReasoningEffort,
	ServerCall,
	ServerNote,
	SubagentChildWire,
	ThreadInfoPayload,
	ThreadListItem,
	ThreadInfoSnapshot,
	UsageBreakdown,
	WireMessage,
	WireMessageUi,
} from '../../../protocol';
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
	usage: UsageBreakdown | null;
	cost: number;
	info: ThreadInfoSnapshot | null;
	branch: string | null;
	/** Plan review awaiting a verdict (ServerCall::PlanVerdict), for the
	 * review card. */
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
	/** Auto-approval verdicts parked before their tool card landed — the
	 * documented `approvalDecision`-before-`toolCall` ordering race; the
	 * `toolCall` upsert drains them onto the fresh card. */
	pendingAutoApprovals: string[];
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
	// Matches the thread-side default; the server replays the persisted effort
	// (and approval mode) on open, correcting these values for restored threads.
	reasoningEffort: 'high',
	approvalMode: 'workspace-write',
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
	pendingAutoApprovals: [],
});
const emptyInfo = (): ThreadInfoSnapshot => ({
	reasoning_effort: 'high',
	cwd_path: null,
	plan: null,
	goal: null,
	usage: {},
	cost: 0,
	pending_auth_count: 0,
	agents: [],
});

/** Merge a typed `ThreadInfoPayload` into the thread's top-level fields and
 * the `info` composite (which plan/goal/usage/git-stats/subagents notes keep
 * filling separately). */
const mergeInfoPayload = (t: ThreadState, p: ThreadInfoPayload): ThreadState => ({
	...t,
	cwd: p.cwd,
	title: p.displayTitle,
	currentModelId: p.modelId,
	approvalMode: p.permissionMode as ApprovalMode,
	reasoningEffort: p.reasoningEffort as ReasoningEffort,
	planMode: p.planMode,
	branch: p.branch,
	info: {
		...(t.info ?? emptyInfo()),
		reasoning_effort: p.reasoningEffort as ReasoningEffort,
		cwd_path: p.cwdPath,
		goal: (p.goal as GoalSnapshotWire | null) ?? (t.info?.goal ?? null),
	},
});

const TERMINAL_TOOL_STATUS = new Set(['completed', 'failed', 'denied', 'cancelled', 'continued']);

const TOOL_OUTPUT_CAP = 64_000;
const capOutputTail = (text: string): string => {
	if (text.length <= TOOL_OUTPUT_CAP) return text;
	let start = text.length - TOOL_OUTPUT_CAP;
	const code = text.charCodeAt(start);
	if (code >= 0xdc00 && code <= 0xdfff) start += 1;
	return text.slice(start);
};

type AskQuestionTranscriptItem = Extract<TranscriptItem, { kind: 'ask_question' }>;

let echoCounter = 0;

const STEER_PENDING_SENTINEL = 'pending';

export class Store {
	private state: ChatState = initialState;
	private readonly listeners = new Set<() => void>();
	/** Drafted sessions not yet confirmed by `SessionCreated`; kept outside
	 * the observable state because it gates input rather than rendering. */
	private readonly creating = new Set<string>();

	subscribe = (listener: () => void): (() => void) => {
		this.listeners.add(listener);
		return () => this.listeners.delete(listener);
	};

	get = (): ChatState => this.state;

	dispatch(msg: FromServer): void {
		// Draft-bookkeeping: a confirmed/disposed session releases its send
		// guard; a global error during draft creation releases every stuck
		// draft and drops the orphan thread states.
		if (msg.kind === 'notification') {
			const note = msg.note;
			if (note.method === 'sessionDisposed') {
				this.creating.delete(note.sessionId);
			} else if (note.method === 'sessionCreated') {
				this.creating.delete(note.sessionId);
			} else if (
				note.method === 'error' &&
				note.sessionId == null &&
				this.creating.size > 0
			) {
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
		}
		this.patch(foldFromServer(this.state, msg));
	}

	/** Seed a fresh thread optimistically from the home composer: switch the
	 * view, echo the first message, and remember the id until `SessionCreated`
	 * confirms the server side. */
	draftThread(sessionId: string, text: string, images?: UserImage[]): void {
		this.creating.add(sessionId);
		this.patch({ ...this.state, view: 'conversation', activeThreadId: sessionId });
		this.echoUser(sessionId, text, images);
	}

	/** Whether a drafted session is still waiting for the server's
	 * `SessionCreated`; sending must stay blocked until it lands. */
	isCreating(sessionId: string): boolean {
		return this.creating.has(sessionId);
	}

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

	clearPlanReview(sessionId: string): void {
		this.patch(
			updateThread(this.state, sessionId, (t) => ({
				...t,
				pendingPlan: null,
				items: t.items.filter((i) => i.kind !== 'plan_review'),
			})),
		);
	}

	/** Switch to a thread, opening it remotely: switch the view and mark
	 * loading until `ThreadHistory` settles. */
	openRemote(sessionId: string): void {
		this.patch({
			...this.state,
			view: 'conversation',
			activeThreadId: sessionId,
			perThread: {
				...this.state.perThread,
				[sessionId]: {
					...(this.state.perThread[sessionId] ?? initThread(sessionId, '')),
					loading: true,
				},
			},
		});
	}

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
		if (t && (t.title !== item.title || t.turnActive !== item.running)) {
			perThread = { ...perThread, [item.id]: { ...t, title: item.title, turnActive: item.running } };
		}
	}
	return { ...state, threads, perThread };
}

/** Fold one `FromServer` (Notification or Request) into state. `Response`
 * frames are dropped by the bridge and never reach here. */
function foldFromServer(state: ChatState, msg: FromServer): ChatState {
	if (msg.kind === 'notification') return foldServerNote(state, msg.note);
	if (msg.kind === 'request') return foldServerCall(state, msg.id, msg.call);
	return state;
}

function foldServerNote(state: ChatState, ev: ServerNote): ChatState {
	// Session-scoped errors stay with their thread; global errors surface at top.
	if (ev.method === 'error') {
		const sessionId = ev.sessionId;
		if (typeof sessionId === 'string') {
			if (!state.perThread[sessionId]) return state;
			return updateThread(state, sessionId, (t) => ({ ...t, error: ev.message }));
		}
		return { ...state, error: ev.message };
	}
	// Global notifications.
	if (!('sessionId' in ev)) {
		switch (ev.method) {
			case 'models':
				return { ...state, models: ev.models };
			case 'threadsUpdated':
				return foldThreads(state, ev.threads);
			case 'commands':
				return { ...state, commands: ev.commands as unknown as CommandEntry[] };
			case 'ready':
				return state;
		}
		return state;
	}
	if (ev.method === 'sessionDisposed') {
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
	// sessionCreated confirms a fresh or opened session; switch the view and
	// clear the draft send guard (the `creating` set is cleared in dispatch).
	if (ev.method === 'sessionCreated') {
		const existing = state.perThread[ev.sessionId];
		const thread = existing ?? initThread(ev.sessionId, '');
		return {
			...state,
			view: 'conversation',
			activeThreadId: ev.sessionId,
			error: null,
			perThread: { ...state.perThread, [ev.sessionId]: thread },
		};
	}
	return updateThread(state, ev.sessionId, (t) => foldThreadNote(t, ev));
}

function foldServerCall(state: ChatState, _id: string, call: ServerCall): ChatState {
	if (!('sessionId' in call)) return state;
	const sessionId = call.sessionId;
	return updateThread(state, sessionId, (t) => {
		switch (call.method) {
			case 'approve': {
				// Upsert: an id already owned by a generic tool item or a prior
				// card is replaced — a server replay must not stack cards.
				const items = t.items.filter(
					(i) => !(i.id === call.authId && (i.kind === 'tool' || i.kind === 'ask_question' || i.kind === 'approval')),
				);
				return {
					...t,
					items: [
						...items,
						{
							kind: 'approval' as const,
							id: call.authId,
							toolName: call.toolName,
							summary: call.summary,
							input: call.input,
						},
					],
				};
			}
			case 'askUserQuestion': {
				const items = t.items.filter(
					(i) => !(i.id === call.authId && (i.kind === 'tool' || i.kind === 'ask_question' || i.kind === 'approval')),
				);
				return {
					...t,
					items: [
						...items,
						{
							kind: 'ask_question' as const,
							id: call.authId,
							summary: '',
							input: call.input,
						},
					],
				};
			}
			case 'planVerdict':
				return upsertPlanReview(t, call.planFile, call.title, call.content ?? '');
			default:
				// BrowserOp / clipboardRead / openExternal: capability seams
				// answered with a Reply from the caller; not surfaced as cards.
				return t;
		}
	});
}

function foldThreadNote(t: ThreadState, ev: ServerNote & { sessionId: string }): ThreadState {
	switch (ev.method) {
		case 'turnStarted':
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
		case 'turnFinished': {
			const stranded = new Set(ev.strandedSteerIds);
			return {
				...t,
				turnActive: false,
				lastTurnDurationSec:
					t.turnStartedAt === null
						? t.lastTurnDurationSec
						: Math.max(0, Math.round((Date.now() - t.turnStartedAt) / 1000)),
				turnStartedAt: null,
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
		case 'steerPending':
			return {
				...t,
				items: t.items.map((i) =>
					i.kind === 'user' && i.clientId === ev.clientId
						? { ...i, steerPendingId: ev.messageId }
						: i,
				),
			};
		case 'steerInjected':
			return {
				...t,
				items: t.items.map((i) =>
					i.kind === 'user' && i.steerPendingId === ev.messageId
						? { ...i, steerPendingId: null }
						: i,
				),
			};
		case 'agentText':
			return appendAssistantText(t, ev.text);
		case 'agentThinking':
			return appendThinkingText(t, ev.text);
		case 'toolCall': {
			if (ev.name === 'AskUserQuestion') {
				const askIdx = t.items.findIndex((i) => i.kind === 'ask_question' && i.id === ev.id);
				if (askIdx === -1) return t;
				const status = foldToolStatus(ev.status);
				if (!TERMINAL_TOOL_STATUS.has(status)) return t;
				const items = t.items.slice();
				items[askIdx] = { ...(items[askIdx] as AskQuestionTranscriptItem), answered: true };
				return { ...t, items };
			}
			let base = t;
			const askIdx = t.items.findIndex((i) => i.kind === 'ask_question' && i.id === ev.id);
			if (askIdx !== -1) {
				const items = t.items.slice();
				items.splice(askIdx, 1);
				base = { ...t, items };
			}
			const drained = base.pendingAutoApprovals.includes(ev.id);
			const next = upsertToolItem(base, ev.id, (prev) => {
				const status = foldToolStatus(ev.status);
				return {
					id: ev.id,
					name: ev.name,
					title:
						prev?.title && ev.title === ev.name
							? prev.title
							: ev.title || prev?.title || ev.name,
					status,
					output: prev?.output ?? '',
					isError: status === 'failed' ? true : (prev?.isError ?? false),
					autoApproved: prev?.autoApproved || drained || undefined,
				};
			});
			return drained
				? { ...next, pendingAutoApprovals: next.pendingAutoApprovals.filter((x) => x !== ev.id) }
				: next;
		}
		case 'toolOutput':
			return upsertToolItem(t, ev.id, (prev) => ({
				id: ev.id,
				name: prev?.name ?? '',
				title: prev?.title ?? ev.id,
				status: prev?.status ?? 'running',
				output: capOutputTail((prev?.output ?? '') + ev.chunk),
				isError: prev?.isError ?? false,
				autoApproved: prev?.autoApproved,
			}));
		case 'toolResult': {
			const askIdx = t.items.findIndex((i) => i.kind === 'ask_question' && i.id === ev.id);
			if (askIdx !== -1) {
				const items = t.items.slice();
				items[askIdx] = {
					...(items[askIdx] as AskQuestionTranscriptItem),
					answered: true,
					output: ev.output,
					isError: ev.isError,
				};
				return { ...t, items };
			}
			return upsertToolItem(t, ev.id, (prev) => ({
				id: ev.id,
				name: prev?.name ?? '',
				title: prev?.title ?? ev.id,
				status: ev.isError
					? 'failed'
					: prev && TERMINAL_TOOL_STATUS.has(prev.status)
						? prev.status
						: 'completed',
				output: capOutputTail(ev.output),
				isError: ev.isError,
				autoApproved: prev?.autoApproved,
			}));
		}
		case 'threadHistory':
			return {
				...t,
				items: wireMessagesToTranscriptItems(
					ev.messages,
					new Set(ev.autoApprovedTools ?? []),
				),
				loading: ev.loading,
				pendingAutoApprovals: [],
			};
		case 'threadInfo':
			return mergeInfoPayload(t, ev.info);
		case 'branch':
			return { ...t, branch: ev.branch };
		case 'gitStats':
			return { ...t, info: { ...(t.info ?? emptyInfo()), git_stats: ev.stats as unknown as never } };
		case 'historyProgress':
			return { ...t, loading: true };
		case 'planModeChanged':
			return { ...t, planMode: ev.enabled };
		case 'planReady':
			return upsertPlanReview(t, ev.planFile, ev.title, ev.content ?? '');
		case 'planUpdated':
			return { ...t, info: { ...(t.info ?? emptyInfo()), plan: ev.snapshot as unknown as never } };
		case 'goalChanged':
			return { ...t, info: { ...(t.info ?? emptyInfo()), goal: ev.snapshot as unknown as never } };
		case 'cwdChanged':
			return {
				...t,
				cwd: ev.path,
				info: { ...(t.info ?? emptyInfo()), cwd_path: ev.path },
			};
		case 'permissionModeChanged':
			return { ...t, approvalMode: ev.mode as ApprovalMode };
		case 'reasoningEffortChanged':
			return { ...t, reasoningEffort: ev.effort as ReasoningEffort };
		case 'currentModel':
			return { ...t, currentModelId: ev.id };
		case 'usage':
			return {
				...t,
				usage: ev.usage as UsageBreakdown,
				cost: ev.cost,
				info: { ...(t.info ?? emptyInfo()), usage: ev.usage as UsageBreakdown, cost: ev.cost },
			};
		case 'usageSnapshot': {
			const info = t.info ?? emptyInfo();
			return {
				...t,
				info: {
					...info,
					cost: ev.cumulativeCost,
					per_model_cost: ev.perModelCost as Record<string, number>,
				},
			};
		}
		case 'compaction':
			return {
				...t,
				items: [
					...t.items,
					{ kind: 'compaction', id: `compaction-${t.items.length}`, summary: ev.summary },
				],
			};
		case 'subagentStarted': {
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
							agent_type: ev.agentType,
							description: ev.description,
							tool_uses: 0,
							latest_activity: null,
							status: 'running',
						},
					],
				},
			};
		}
		case 'subagentProgress': {
			const info = t.info ?? emptyInfo();
			const exists = info.agents.some((a) => a.id === ev.id);
			const updated = exists
				? info.agents.map((a) =>
						a.id === ev.id
							? {
								...a,
								tool_uses: ev.toolUses,
								latest_activity: ev.latestActivity,
								status: ev.status as never,
							}
							: a,
					)
				: [
						...info.agents,
						{
							id: ev.id,
							agent_type: ev.agentType,
							description: '',
							tool_uses: ev.toolUses,
							latest_activity: ev.latestActivity,
							status: ev.status as never,
						},
					];
			return { ...t, info: { ...info, agents: updated } };
		}
		case 'backgroundTaskUpdated': {
			const task = ev.snapshot as unknown as BackgroundTaskSnapshotWire;
			const known = task.task_id in t.backgroundTasks;
			const backgroundTasks = { ...t.backgroundTasks, [task.task_id]: task };
			const items: TranscriptItem[] = known
				? t.items
				: [...t.items, { kind: 'background_task', id: `bg-${task.task_id}`, task }];
			return { ...t, backgroundTasks, items };
		}
		case 'subagentChild': {
			const prior = t.subagentChildren[ev.id] ?? [];
			const next = [...prior, ev.event as SubagentChildWire].slice(-200);
			return { ...t, subagentChildren: { ...t.subagentChildren, [ev.id]: next } };
		}
		case 'approvalDecision': {
			if (ev.verdict !== 'allow') return t;
			const exists = t.items.some((i) => i.kind === 'tool' && i.id === ev.toolCallId);
			if (!exists) {
				return { ...t, pendingAutoApprovals: [...t.pendingAutoApprovals, ev.toolCallId] };
			}
			return {
				...t,
				items: t.items.map((i) =>
					i.kind === 'tool' && i.id === ev.toolCallId
						? { ...i, tool: { ...i.tool, autoApproved: true } }
						: i,
				),
			};
		}
		default:
			return t;
	}
}

function upsertPlanReview(
	t: ThreadState,
	planFile: string,
	title: string,
	content: string,
): ThreadState {
	const items = t.items.filter((i) => i.kind !== 'plan_review');
	return {
		...t,
		pendingPlan: { planFile, title, content },
		items: [
			...items,
			{
				kind: 'plan_review',
				id: `plan-review-${items.length}`,
				planFile,
				title,
				content,
			},
		],
	};
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

export function wireMessagesToTranscriptItems(
	messages: WireMessage[],
	autoApproved?: ReadonlySet<string>,
): TranscriptItem[] {
	const items: TranscriptItem[] = [];
	const toolNames = new Map<string, string>();
	for (const msg of messages) {
		const ui: Partial<WireMessageUi> = msg.ui ?? {};
		if (msg.role === 'user') {
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
				author: ui.author ?? null,
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
							autoApproved: autoApproved?.has(block.ToolUse.id) || undefined,
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
					autoApproved: autoApproved?.has(result.tool_use_id) || undefined,
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
