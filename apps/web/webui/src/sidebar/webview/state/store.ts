// Multi-thread chat store, v2 (§F.2). One fold path maps `FromServer` v2
// frames into immutable state:
//
//   J  journal entries   → `TranscriptFold` (entries.ts) → `items`
//   P  projections       → per-key {value, seq} higher-seq-wins → typed fields
//   Q  GetConversationInfo (on-demand fold) — §E.3: the store carries the
//      durable committed-message count (`committed`, the refresh edge
//      signal) and the `setConversationInfo` seam the conversation-info
//      plugin writes through; the pull itself is plugin-owned (T8 §H)
//   H  host events       → global registries + SessionStatus monotonic mirror
//   client-owned         → view selection, drafts, echoes, focus/unread/errored
//
// Every live session is driven by a `JournalStream` engine (T3): the bridge
// routes `StreamItem`/`StreamEnd` frames by `stream_id` into `dispatch`; the
// engine accepts/rejects entries by seq (idempotent drops, gap detection),
// the store keeps accepted records in a seq-ordered cache, and the fold
// re-derives `items` from the window. Optimistic echoes ride alongside the
// window (client-owned) and are retired when the durable user entry carrying
// their `originRpc` arrives (§F.2 echo retirement). L6: no domain parsing —
// values arrive whole from entries or projections. Selection state is never
// read back from a mirror (T2③ root-cause fix).
//
// Reconnect / resync: on a connection reseat (same persisted `client_id`) or
// a `StreamEnd{Resync}` / engine violation, the engine `restart()`s (the next
// snapshot is validated as a resume, §F.1 rule 3 — the old window stays
// published until the new snapshot lands) and the stream is re-opened. The
// engine's synchronous gap-repair source stays empty on the webui (pages
// arrive asynchronously), so a detected gap takes the L5 fallback: the engine
// reports the violation and the store re-follows with a fresh snapshot.
//
// v1 death path: domain `ServerNote` arms (turn/tool/text/history/usage/
// threadInfo/…) are §D.6 doomed and ignored; the only notes the store still
// reads are the global registry/control set the server still emits at wave/2
// (`models` / `threadsUpdated` / `commands` / `ready` / `error` /
// `sessionCreated` / `sessionDisposed`), each mirrored through the matching
// `HostEvent` tag so the store is dual-path ready for the T10 envelope swap.
// `Response` frames are resolved against the bridge's pending table
// (receipts) and never reach the store.

import type {
	ApprovalMode,
	BackgroundTaskSnapshotWire,
	CommandEntry,
	ConversationInfo,
	FromServer,
	GoalSnapshotWire,
	ModelInfo,
	PlanSnapshotWire,
	ReasoningEffort,
	ServerCall,
	ServerNote,
	SubagentChildWire,
	ThreadListItem,
} from '../../../protocol';
import { isKnownJournalTag, parseHostEvent } from '../../../../../../../crates/manox-protocol/bindings/guards';
import { JournalStream, type JournalChange, type JournalEntry } from './journal';
import {
	normalizeWireRecords,
	TranscriptFold,
	type WireRecord,
} from './entries';
import type { TranscriptItem, UserImage } from './transcript';

export type { ToolCallState, ToolUiStatus, TranscriptItem, UserImage } from './transcript';

/** One §E.2 projection key slot: whole value + the seq that produced it. */
interface ProjectionSlot {
	value: unknown;
	seq: number;
}

/** Cached journal row: the §C.1 envelope record plus decoded bookkeeping. */
interface WindowRecord {
	record: WireRecord;
	/** Durable origin of a `message{role:user}` row (echo retirement key). */
	originRpc: string | null;
	/** Durable user/assistant message rows count toward the §E.3 refresh
	 * edge signal. */
	committed: boolean;
}

/** Client-owned optimistic bubble. */
interface Echo {
	key: string;
	text: string;
	images?: UserImage[];
	/** RPC id of the `Submit` the echo rides on; the durable user entry with
	 * the same `originRpc` retires it (§F.2). */
	originRpc: string | null;
	queued?: boolean;
	/** The echo was converted into a steer of the running turn (client-owned
	 * chip; cleared / failed-flagged on the next `turnFinish` row's
	 * stranded ids). */
	steerPendingId?: string | null;
	steerFailed?: boolean;
	timestamp: number;
	modelRef: string | null;
}

/** A `PageHistory` resolution (§D.2): dense rows + the older-exists flag. */
export interface JournalPageData {
	records: WireRecord[];
	hasMore: boolean;
}

/** The effects seam: the store never posts frames itself (pure fold); the
 * api client installs the real transport, tests install recorders. */
export interface StoreEffects {
	/** Re-open a follow stream for `sessionId` under `streamId`. */
	openStream(sessionId: string, streamId: string): void;
	/** `PageHistory` through `throughSeq` (inclusive); resolve the page. */
	pageHistory(sessionId: string, throughSeq: number): Promise<JournalPageData | null>;
}

export interface ThreadState {
	sessionId: string;
	/** A snapshot has arrived (session_ready derives from the Snapshot
	 * frame — never from a control note). */
	ready: boolean;
	cwd: string;
	/** Display title from the thread registry / `title` projection. */
	title: string;
	/** A turn is in flight (projection `running`, latest-wins). */
	turnActive: boolean;
	/** Plan mode (read-only research) — the engine's sidecar mirror. */
	planMode: boolean;
	items: TranscriptItem[];
	/** Canonical `{provider}/{modelId}` display ref from the `model`
	 * projection (L8); the picker reads it — no bare-id resolution. */
	modelRef: string | null;
	approvalMode: ApprovalMode;
	reasoningEffort: ReasoningEffort;
	branch: string | null;
	plan: PlanSnapshotWire | null;
	goal: GoalSnapshotWire | null;
	/** Pending approval auth ids (projection `pending_auth` keys). */
	pendingAuthIds: string[];
	/** Live background-task snapshots keyed by task id (fold-derived). */
	backgroundTasks: Record<string, BackgroundTaskSnapshotWire>;
	/** Sub-agent progress rows (fold-derived). */
	subagents: Array<{
		id: string;
		agent_type: string;
		description: string;
		tool_uses: number;
		latest_activity: string | null;
		status: string;
	}>;
	/** Streamed child-session events per sub-agent (fold-derived). */
	subagentChildren: Record<string, SubagentChildWire[]>;
	/** The §E.3 Q-face payload (spend tree / lifetime tokens / context
	 * budget). Written through the `setConversationInfo` seam by the
	 * conversation-info plugin (T8 §H); the store only owns the edge
	 * signal (`committed`). This replaces the doomed GetUsage /
	 * UsageSnapshot / ThreadInfo request-note path. */
	conversationInfo: ConversationInfo | null;
	/** Durable user/assistant `message` rows inside the published journal
	 * window — the §E.3 refresh edge signal (§E.3: the client pulls the Q
	 * face only when this advances). */
	committed: number;
	/** Restored history still loading (open → first snapshot). */
	loading: boolean;
	/** Last error emitted for this thread; cleared when a new turn starts. */
	error: string | null;
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
	/** Transport generation (`ready` arrivals): drives the reconnect
	 * affordance. */
	connected: number;
}

const initialState: ChatState = {
	view: 'threads',
	threads: [],
	activeThreadId: null,
	perThread: {},
	models: [],
	commands: [],
	error: null,
	connected: 0,
};

const initThread = (sessionId: string): ThreadState => ({
	sessionId,
	ready: false,
	cwd: '',
	title: 'New conversation',
	turnActive: false,
	planMode: false,
	items: [],
	modelRef: null,
	// Matches the thread-side default; the snapshot's projection baseline
	// corrects these values for restored threads.
	reasoningEffort: 'high',
	approvalMode: 'workspace-write',
	branch: null,
	plan: null,
	goal: null,
	pendingAuthIds: [],
	backgroundTasks: {},
	subagents: [],
	subagentChildren: {},
	conversationInfo: null,
	committed: 0,
	loading: false,
	error: null,
	lastTurnDurationSec: null,
});

const isRecord = (v: unknown): v is Record<string, unknown> =>
	typeof v === 'object' && v !== null && !Array.isArray(v);
const asString = (v: unknown): string | null => (typeof v === 'string' ? v : null);
const asNumber = (v: unknown): number | null =>
	typeof v === 'number' && Number.isFinite(v) ? v : null;
const asBool = (v: unknown): boolean | null => (typeof v === 'boolean' ? v : null);

/** Wire `permission_mode` normalization: the projection seeds snake_case
 * (`workspace_write`), the `PermissionModeChange` fold row carries the
 * engine's kebab wire form (`workspace-write`). Both map to the client
 * `ApprovalMode` union; an unrecognized value falls back to the default
 * (L12 tolerance — never crash the approval chip on wire drift). */
const APPROVAL_MODES: ReadonlySet<string> = new Set([
	'read-only',
	'workspace-write',
	'danger-full-access',
]);
const kebabMode = (mode: unknown): ApprovalMode => {
	const kebab = (asString(mode) ?? '').replaceAll('_', '-');
	return (APPROVAL_MODES.has(kebab) ? kebab : 'workspace-write') as ApprovalMode;
};

/** The `model` projection value `{provider, modelId}` → canonical display
 * ref (L8: clients never resolve identity, only split for display). */
const modelRefOf = (value: unknown): string | null => {
	if (!isRecord(value)) return null;
	const provider = asString(value.provider);
	const modelId = asString(value.modelId);
	if (!provider || !modelId) return null;
	return `${provider}/${modelId}`;
};

/** Client-owned ephemeral adjudication card (from a `ServerCall`). Lives
 * next to the fold window (not inside it): a rebuilt journal window must
 * never drop a card awaiting the user's verdict. */
type TransientCard =
	| { kind: 'approval'; id: string; toolName: string; summary: string; input?: unknown }
	| { kind: 'ask_question'; id: string; summary: string; input: unknown; answered?: boolean }
	| { kind: 'plan_review'; id: string; planFile: string; title: string; content: string };

/** The per-session runtime: engine + fold + row cache + projection slots +
 * client-owned echoes. Lives outside the observable state; `items` / typed
 * fields are derived views over it (L9: one source, no mirror copy). */
interface Runtime {
	engine: JournalStream;
	fold: TranscriptFold;
	/** Accepted rows, ascending seq (binary-inserted). */
	rows: WindowRecord[];
	projections: Map<string, ProjectionSlot>;
	cards: TransientCard[];
	streamId: string | null;
	openPending: boolean;
	echoes: Echo[];
	echoSeq: number;
	/** committed = durable user/assistant `message` rows in the window: the
	 * §E.3 refresh edge signal (promoted onto `ThreadState`; the
	 * conversation-info plugin watches it and pulls the Q face). */
	committed: number;
	paging: boolean;
	/** Wall-clock of the in-flight turn (running edge), for the duration. */
	turnStartedAt: number | null;
	lastTurnDurationSec: number | null;
}

let streamCounter = 0;
const nextStreamId = (): string => `webui-stream-${++streamCounter}`;

export class Store {
	private state: ChatState = initialState;
	private readonly listeners = new Set<() => void>();
	private readonly sessions = new Map<string, Runtime>();
	/** Drafted sessions not yet confirmed by `sessionCreated`; kept outside
	 * the observable state because it gates input rather than rendering. */
	private readonly creating = new Set<string>();
	private effects: StoreEffects = {
		openStream: () => undefined,
		pageHistory: async () => null,
	};

	subscribe = (listener: () => void): (() => void) => {
		this.listeners.add(listener);
		return () => this.listeners.delete(listener);
	};

	get = (): ChatState => this.state;

	/** Install the transport effects (called once by the api client). */
	attachEffects(effects: StoreEffects): void {
		this.effects = effects;
	}

	dispatch(msg: FromServer): void {
		switch (msg.kind) {
			case 'streamItem':
				this.onStreamItem(msg.streamId, msg.frame);
				return;
			case 'streamEnd':
				this.onStreamEnd(msg.streamId, msg.reason);
				return;
			case 'host':
				this.onHostEvent(msg.host);
				return;
			case 'notification':
				// Domain `ServerNote` arms are dead (§D.6); the store reads
				// only the global registry/control set (see `onServerNote`).
				this.onServerNote(msg.note);
				return;
			case 'request':
				this.onServerCall(msg.id, msg.call);
				return;
			case 'response':
				// Resolved against the bridge's pending table (receipts)
				// before dispatch; strays are dropped by the bridge.
				return;
		}
	}

	// ── session lifecycle (client → store) ────────────────────────────────

	/** Seed a fresh thread optimistically from the home composer: switch the
	 * view, echo the first message, and remember the id until
	 * `sessionCreated` confirms the server side. `sessionId` is the
	 * client-minted draft id; `confirmDraft` rekeys it to the canonical id
	 * the `CreateSession` receipt answers with. */
	draftThread(
		sessionId: string,
		text: string,
		images?: UserImage[],
		opts: { originRpc?: string } = {},
	): void {
		this.creating.add(sessionId);
		const runtime = this.ensureRuntime(sessionId);
		this.patch({
			...this.state,
			view: 'conversation',
			activeThreadId: sessionId,
			perThread: {
				...this.state.perThread,
				[sessionId]: this.deriveThread(sessionId, initThread(sessionId), runtime),
			},
		});
		if (text || images?.length) this.echoUser(sessionId, text, images, opts);
	}

	/** Rekey a client-minted draft id to the server's canonical session id
	 * (the `CreateSession` receipt, §D.2). */
	confirmDraft(localId: string, serverId: string): void {
		if (localId === serverId) return;
		const runtime = this.sessions.get(localId);
		if (!runtime) return;
		this.sessions.delete(localId);
		this.sessions.set(serverId, runtime);
		this.creating.delete(localId);
		this.creating.add(serverId);
		const perThread = { ...this.state.perThread };
		const thread = perThread[localId];
		if (thread) {
			perThread[serverId] = { ...thread, sessionId: serverId };
			delete perThread[localId];
		}
		this.patch({
			...this.state,
			perThread,
			activeThreadId:
				this.state.activeThreadId === localId ? serverId : this.state.activeThreadId,
		});
	}

	/** Whether a drafted session is still waiting for the server's
	 * `sessionCreated`; sending must stay blocked until it lands. */
	isCreating(sessionId: string): boolean {
		return this.creating.has(sessionId);
	}

	/** Optimistic echo of a submission; retired by the durable user entry
	 * carrying the same `originRpc` (§F.2). */
	echoUser(
		sessionId: string,
		text: string,
		images?: UserImage[],
		opts: { queued?: boolean; originRpc?: string; steerPendingId?: string } = {},
	): void {
		const runtime = this.ensureRuntime(sessionId);
		runtime.echoes = [
			...runtime.echoes,
			{
				key: `echo-${sessionId}-${++runtime.echoSeq}`,
				text,
				images: images?.length ? images : undefined,
				originRpc: opts.originRpc ?? null,
				queued: opts.queued,
				steerPendingId: opts.steerPendingId ?? null,
				timestamp: Math.round(Date.now() / 1000),
				modelRef: modelRefOf(this.projectionValue(runtime, 'model')),
			},
		];
		this.publishThread(sessionId);
	}

	removeUser(sessionId: string, clientId: string): void {
		const runtime = this.sessions.get(sessionId);
		if (!runtime) return;
		runtime.echoes = runtime.echoes.filter(
			(e) => e.key !== clientId && e.originRpc !== clientId,
		);
		this.publishThread(sessionId);
	}

	markSteerPending(sessionId: string, clientId: string): void {
		const runtime = this.sessions.get(sessionId);
		if (!runtime) return;
		runtime.echoes = runtime.echoes.map((e) =>
			e.key === clientId || e.originRpc === clientId
				? { ...e, steerPendingId: clientId, queued: false }
				: e,
		);
		this.publishThread(sessionId);
	}

	decideApproval(sessionId: string, id: string): void {
		const runtime = this.sessions.get(sessionId);
		if (runtime) {
			runtime.cards = runtime.cards.filter((c) => c.id !== id);
		}
		this.publishThread(sessionId);
	}

	respondAsk(sessionId: string, id: string): void {
		const runtime = this.sessions.get(sessionId);
		if (runtime) {
			runtime.cards = runtime.cards.map((c) =>
				c.kind === 'ask_question' && c.id === id ? { ...c, answered: true } : c,
			);
		}
		this.publishThread(sessionId);
	}

	clearPlanReview(sessionId: string): void {
		const runtime = this.sessions.get(sessionId);
		if (runtime) {
			runtime.cards = runtime.cards.filter((c) => c.kind !== 'plan_review');
		}
		this.publishThread(sessionId);
	}

	/** Switch to a thread, opening it remotely: switch the view and mark
	 * loading until the first snapshot lands (§F.2: history arrives via
	 * the follow stream; the `OpenSession` receipt is just ownership). */
	openRemote(sessionId: string): void {
		this.clearUnreadRow(sessionId);
		const runtime = this.ensureRuntime(sessionId);
		runtime.openPending = true;
		this.patch({
			...this.state,
			view: 'conversation',
			activeThreadId: sessionId,
			perThread: {
				...this.state.perThread,
				[sessionId]: this.deriveThread(
					sessionId,
					{
						...(this.state.perThread[sessionId] ?? initThread(sessionId)),
						loading: true,
					},
					runtime,
				),
			},
		});
		this.effects.openStream(sessionId, this.ensureStreamId(runtime));
	}

	openLocal(sessionId: string): void {
		this.clearUnreadRow(sessionId);
		this.patch({ ...this.state, view: 'conversation', activeThreadId: sessionId });
	}

	backToList(): void {
		this.patch({ ...this.state, view: 'threads' });
	}

	/** Session ids with live local state (the api re-follows these on a
	 * transport reseat). */
	liveSessionIds(): string[] {
		return [...this.sessions.keys()];
	}

	/** The follow stream id in use for `sessionId`, if any. */
	streamIdOf(sessionId: string): string | null {
		return this.sessions.get(sessionId)?.streamId ?? null;
	}

	/** Mark the transport generation change: every engine restarts (the next
	 * snapshot is validated as a resume, §F.1 rule 3) and the streams are
	 * re-opened. The old windows stay published until the snapshots land. */
	reseat(): void {
		for (const [sessionId, runtime] of this.sessions) {
			runtime.engine.restart();
			runtime.openPending = true;
			// Rotate the stream id for the new connection generation
			// (§D.1 "unique per connection"): a late `StreamEnd{Closed}` or
			// snapshot from the *old* generation carries the previous id, so
			// `sessionOfStream` ignores it and the fresh snapshot lands on
			// the new id — a re-seat race the client cannot otherwise close.
			runtime.streamId = nextStreamId();
			this.effects.openStream(sessionId, runtime.streamId);
		}
	}

	/** §D.5: focus clears the row's unread flag (selection is client-owned). */
	private clearUnreadRow(sessionId: string): void {
		const idx = this.state.threads.findIndex((r) => r.id === sessionId);
		if (idx < 0 || !this.state.threads[idx]?.unread) return;
		const threads = this.state.threads.slice();
		threads[idx] = { ...threads[idx] as ThreadListItem, unread: false };
		this.patch({ ...this.state, threads });
	}

	private ensureStreamId(runtime: Runtime): string {
		if (runtime.streamId === null) runtime.streamId = nextStreamId();
		return runtime.streamId;
	}

	private patch(next: ChatState): void {
		if (next === this.state) return;
		this.state = next;
		for (const listener of this.listeners) listener();
	}

	// ── stream routing (bridge → store) ───────────────────────────────────

	private sessionOfStream(streamId: string): [string, Runtime] | null {
		for (const entry of this.sessions.entries()) {
			if (entry[1].streamId === streamId) return entry;
		}
		return null;
	}

	private onStreamItem(streamId: string, frame: unknown): void {
		const hit = this.sessionOfStream(streamId);
		if (!hit) {
			console.warn('[webui] StreamItem for unknown stream', streamId);
			return;
		}
		const [sessionId, runtime] = hit;
		if (!isRecord(frame) || typeof frame.type !== 'string') {
			console.warn('[webui] malformed stream frame dropped', frame);
			return;
		}
		switch (frame.type) {
			case 'snapshot':
				this.onSnapshot(sessionId, runtime, frame);
				return;
			case 'entry':
				this.onEntryFrame(sessionId, runtime, frame);
				return;
			case 'projections':
				this.onProjections(sessionId, runtime, frame);
				return;
			default:
				console.warn('[webui] unknown stream frame type', frame.type);
				return;
		}
	}

	private onSnapshot(sessionId: string, runtime: Runtime, frame: Record<string, unknown>): void {
		const cursor = asNumber(frame.cursor) ?? 0;
		const rows = normalizeWireRecords(frame.records).map((record) => ({
			record,
			originRpc: originOf(record),
			committed: isCommittedRow(record),
		}));
		const page: JournalEntry[] = rows.map((row) => ({
			first: row.record.seq,
			last: row.record.seq,
		}));
		for (const row of rows) insertRow(runtime.rows, row);
		// The wire `cursor` is the entry count (dense, 0-based ⇒ exclusive
		// end); the engine's §F.1 convention is the inclusive page tail —
		// for a non-empty window that is the last record's seq. An empty
		// snapshot keeps the wire cursor (the engine treats it as the
		// dsh `emptyCursor` case and leaves the tail `undefined`).
		const engineCursor =
			page.length > 0 ? (page[page.length - 1] as JournalEntry).last : cursor;
		const violation = runtime.engine.opened(engineCursor, page);
		if (violation) return; // `failed` already scheduled the resync
		runtime.openPending = false;
		const projections = isRecord(frame.projections) ? frame.projections : {};
		this.storeProjections(
			runtime,
			projections,
			asNumber(frame.projectionsAsOfSeq) ?? cursor,
		);
		// Transcript baseline: seed the running model pointer from the
		// `model` projection (the fold advances it on `modelChange` rows),
		// re-fold the whole window, then the projection-visible artifacts.
		runtime.fold.modelRef = modelRefOf(this.projectionValue(runtime, 'model'));
		this.rebuildFold(sessionId, runtime);
		const header = isRecord(frame.header) ? frame.header : null;
		this.patch(
			updateThread(this.state, sessionId, (t) => ({
				...t,
				loading: false,
				ready: true,
				cwd: asString(header?.cwd) ?? t.cwd,
			})),
		);
	}

	private onEntryFrame(
		sessionId: string,
		runtime: Runtime,
		frame: Record<string, unknown>,
	): void {
		const seq = asNumber(frame.seq);
		const event = isRecord(frame.event) ? frame.event : null;
		if (seq === null || !event) {
			console.warn('[webui] malformed entry frame dropped', frame);
			return;
		}
		if (!isKnownJournalTag(event.type)) {
			// L12 tolerance: drop + log, never disconnect.
			console.warn('[webui] unknown journal entry tag dropped', event.type);
			return;
		}
		const record: WireRecord = {
			...event,
			seq,
			type: event.type,
			id: `e-${seq}`,
			timestamp: new Date().toISOString(),
		};
		const violation = runtime.engine.entry({ first: seq, last: seq });
		if (violation) return; // engine violation → `failed` → resync
		const tail = runtime.engine.cursors().last;
		if (tail === undefined || tail < seq) return; // not applied (gap)
		insertRow(runtime.rows, {
			record,
			originRpc: originOf(record),
			committed: isCommittedRow(record),
		});
		runtime.fold.append(record);
		// A live durable user entry can retire its echo mid-stream (§F.2).
		this.retireEchoes(runtime);
		this.applyFoldSide(sessionId, runtime);
		this.trackCommitted(sessionId, runtime);
		this.publishThread(sessionId);
	}

	private onProjections(
		sessionId: string,
		runtime: Runtime,
		frame: Record<string, unknown>,
	): void {
		const asOfSeq = asNumber(frame.asOfSeq) ?? 0;
		const values = isRecord(frame.values) ? frame.values : {};
		this.storeProjections(runtime, values, asOfSeq);
		this.tickRunning(sessionId, runtime);
		this.publishThread(sessionId);
	}

	private onStreamEnd(streamId: string, reason: unknown): void {
		const hit = this.sessionOfStream(streamId);
		if (!hit) return;
		const [sessionId, runtime] = hit;
		const type = isRecord(reason) ? asString(reason.type) : null;
		switch (type) {
			case 'resync':
				// L5: re-follow. The engine keeps its old window published
				// until the fresh snapshot lands (seamless reconnect).
				this.resync(sessionId, runtime);
				return;
			case 'failure': {
				const message = isRecord(reason) ? asString(reason.message) : null;
				console.warn('[webui] stream failure', sessionId, message);
				this.patch(
					updateThread(this.state, sessionId, (t) => ({
						...t,
						error: message ?? 'stream failure',
					})),
				);
				return;
			}
			case 'closed':
			case 'cancelled':
				// Ownership lost / explicit cancel: drop the local runtime;
				// reopening goes through `openThread`.
				this.sessions.delete(sessionId);
				return;
			default:
				console.warn('[webui] unknown stream end reason', reason);
				return;
		}
	}

	// ── host / note folds ─────────────────────────────────────────────────

	private onHostEvent(host: unknown): void {
		const guard = parseHostEvent(host);
		if (!guard.ok) {
			console.warn('[webui] host event dropped:', guard.reason);
			return;
		}
		const ev = guard.value as Record<string, unknown>;
		switch (ev.type) {
			case 'ready':
				this.patch({ ...this.state, connected: this.state.connected + 1 });
				return;
			case 'models': {
				const models = Array.isArray(ev.models) ? (ev.models as ModelInfo[]) : null;
				if (models) this.patch({ ...this.state, models });
				return;
			}
			case 'commands': {
				const commands = Array.isArray(ev.commands) ? (ev.commands as CommandEntry[]) : null;
				if (commands) this.patch({ ...this.state, commands });
				return;
			}
			case 'threadsUpdated': {
				const threads = Array.isArray(ev.threads) ? (ev.threads as ThreadListItem[]) : null;
				if (threads) this.patch(foldThreads(this.state, threads));
				return;
			}
			case 'sessionStatus':
				this.mirrorSessionStatus(ev);
				return;
			case 'sessionCreated':
				this.onServerNote({ method: 'sessionCreated', sessionId: asString(ev.sessionId) ?? '' });
				return;
			case 'sessionDisposed':
				this.onServerNote({
					method: 'sessionDisposed',
					sessionId: asString(ev.sessionId) ?? '',
				});
				return;
			case 'error':
				this.onServerNote({
					method: 'error',
					sessionId: null,
					message: asString(ev.message) ?? '',
				});
				return;
			default:
				return;
		}
	}

	/** §D.5 monotonic mirror rules onto the threads-list row flags:
	 * `running` latest-wins, `errored` sets on true edges (a cleared flag
	 * only comes back with the list snapshot), `unread` only increases until
	 * focus, the rest latest-wins. The row's persisted columns
	 * (title/pin/archive/model) belong to `ThreadsUpdated` and are kept. */
	private mirrorSessionStatus(ev: Record<string, unknown>): void {
		const sessionId = asString(ev.sessionId);
		if (!sessionId) return;
		const active = this.state.activeThreadId === sessionId;
		const running = asBool(ev.running);
		const errored = asBool(ev.errored);
		const unread = asBool(ev.unread);
		const pendingAuth = asBool(ev.pendingAuth);
		const pendingPlan = asBool(ev.pendingPlan);
		const backgroundWork = asBool(ev.backgroundWork);
		let changed = false;
		const threads = this.state.threads.map((row) => {
			if (row.id !== sessionId) return row;
			const next: ThreadListItem = {
				...row,
				...(running !== null ? { running } : {}),
				...(errored === true ? { errored: true } : {}),
				...(unread === true && !active ? { unread: true } : {}),
				...(unread === false && active ? { unread: false } : {}),
				...(pendingAuth !== null ? { pending_auth: pendingAuth } : {}),
				...(pendingPlan !== null ? { pending_plan: pendingPlan } : {}),
				...(backgroundWork !== null ? { background_work: backgroundWork } : {}),
			};
			if (JSON.stringify(next) !== JSON.stringify(row)) changed = true;
			return next;
		});
		if (changed) this.patch({ ...this.state, threads });
	}

	/** v1 global registry/control notes the server still emits at wave/2. */
	private onServerNote(note: ServerNote): void {
		switch (note.method) {
			case 'error': {
				const sessionId = note.sessionId;
				if (typeof sessionId === 'string' && this.state.perThread[sessionId]) {
					this.patch(
						updateThread(this.state, sessionId, (t) => ({ ...t, error: note.message })),
					);
					return;
				}
				if (typeof sessionId !== 'string') {
					// A global error during draft creation releases every
					// stuck draft and drops the orphan thread states.
					if (this.creating.size > 0) {
						const stuck = [...this.creating];
						this.creating.clear();
						const perThread = { ...this.state.perThread };
						for (const id of stuck) {
							delete perThread[id];
							this.sessions.delete(id);
						}
						const active = this.state.activeThreadId;
						this.patch({
							...this.state,
							perThread,
							error: note.message,
							...(active !== null && stuck.includes(active)
								? { view: 'threads' as const, activeThreadId: null }
								: {}),
						});
						return;
					}
					this.patch({ ...this.state, error: note.message });
				}
				return;
			}
			case 'sessionDisposed': {
				const perThread = { ...this.state.perThread };
				delete perThread[note.sessionId];
				this.sessions.delete(note.sessionId);
				this.creating.delete(note.sessionId);
				const wasActive = this.state.activeThreadId === note.sessionId;
				this.patch({
					...this.state,
					perThread,
					activeThreadId: wasActive ? null : this.state.activeThreadId,
					view: wasActive ? 'threads' : this.state.view,
				});
				return;
			}
			case 'sessionCreated': {
				const sessionId = note.sessionId;
				this.creating.delete(sessionId);
				if (this.state.perThread[sessionId]) {
					this.patch({
						...this.state,
						view: 'conversation',
						activeThreadId: sessionId,
						error: null,
					});
					return;
				}
				const runtime = this.ensureRuntime(sessionId);
				this.patch({
					...this.state,
					view: 'conversation',
					activeThreadId: sessionId,
					error: null,
					perThread: {
						...this.state.perThread,
						[sessionId]: this.deriveThread(sessionId, initThread(sessionId), runtime),
					},
				});
				return;
			}
			case 'models':
				this.patch({ ...this.state, models: note.models });
				return;
			case 'commands':
				this.patch({ ...this.state, commands: note.commands as unknown as CommandEntry[] });
				return;
			case 'threadsUpdated':
				this.patch(foldThreads(this.state, note.threads));
				return;
			case 'ready':
				this.patch({ ...this.state, connected: this.state.connected + 1 });
				return;
			default:
				// §D.6 doomed domain note — the v2 successor is the journal
				// stream; silently ignored during the migration window.
				return;
		}
	}

	private onServerCall(_id: string, call: ServerCall): void {
		if (!('sessionId' in call)) return;
		const sessionId = call.sessionId;
		const runtime = this.ensureRuntime(sessionId);
		switch (call.method) {
			case 'approve': {
				// Upsert: an id already owned by a prior card is replaced —
				// a server replay must not stack cards.
				runtime.cards = [
					...runtime.cards.filter((c) => c.id !== call.authId),
					{
						kind: 'approval' as const,
						id: call.authId,
						toolName: call.toolName,
						summary: call.summary,
						input: call.input,
					},
				];
				break;
			}
			case 'askUserQuestion': {
				runtime.cards = [
					...runtime.cards.filter((c) => c.id !== call.authId),
					{ kind: 'ask_question' as const, id: call.authId, summary: '', input: call.input },
				];
				break;
			}
			case 'planVerdict': {
				runtime.cards = [
					...runtime.cards.filter((c) => c.kind !== 'plan_review'),
					{
						kind: 'plan_review' as const,
						id: `plan-review-${call.planFile}`,
						planFile: call.planFile,
						title: call.title,
						content: call.content ?? '',
					},
				];
				break;
			}
			default:
				// BrowserOp / clipboardRead / openExternal: capability seams
				// answered with a Reply from the caller; not surfaced as cards.
				return;
		}
		this.publishThread(sessionId);
	}

	// ── engine plumbing ───────────────────────────────────────────────────

	private ensureRuntime(sessionId: string): Runtime {
		let runtime = this.sessions.get(sessionId);
		if (runtime) return runtime;
		const store = this;
		const engine = new JournalStream(
			{
				name: sessionId,
				// The engine's synchronous gap-repair source stays empty on
				// the webui (PageHistory resolves asynchronously): a gap
				// surfaces as an engine violation → `failed` → re-follow
				// (the L5 fallback).
				readPage: () => [],
			},
			{
				publish: (change) => store.onPublish(sessionId, runtime as Runtime, change),
				failed: (message) => {
					console.error('[webui] journal engine violation:', message);
					store.resync(sessionId, runtime as Runtime);
				},
			},
		);
		runtime = {
			engine,
			fold: new TranscriptFold(),
			rows: [],
			projections: new Map(),
			cards: [],
			streamId: null,
			openPending: false,
			echoes: [],
			echoSeq: 0,
			committed: 0,
			paging: false,
			turnStartedAt: null,
			lastTurnDurationSec: null,
		};
		this.sessions.set(sessionId, runtime);
		return runtime;
	}

	private resync(sessionId: string, runtime: Runtime): void {
		runtime.engine.restart();
		runtime.openPending = true;
		this.effects.openStream(sessionId, this.ensureStreamId(runtime));
	}

	private onPublish(sessionId: string, runtime: Runtime, change: JournalChange): void {
		switch (change.type) {
			case 'append':
				// Entry frames are applied inline in `onEntryFrame`; the
				// publish is bookkeeping only.
				return;
			case 'replace':
			case 'prepend':
				// The engine's window just moved (opening or a prepend
				// page). Rebuild the transcript from the row cache.
				this.rebuildFold(sessionId, runtime);
				return;
		}
	}

	/** Re-fold `items` from the engine-published window (cached rows inside
	 * the cursors range) plus projection-visible artifacts, then refresh the
	 * committed edge signal. */
	private rebuildFold(sessionId: string, runtime: Runtime): void {
		const cursors = runtime.engine.cursors();
		const first = cursors.first;
		const last = cursors.last;
		runtime.fold.modelRef =
			modelRefOf(this.projectionValue(runtime, 'model')) ?? runtime.fold.modelRef;
		const rows =
			first === undefined || last === undefined
				? []
				: runtime.rows.filter((r) => r.record.seq >= first && r.record.seq <= last);
		runtime.fold.replace(rows.map((r) => r.record));
		runtime.fold.seedProjections(this.projectionValues(runtime));
		this.retireEchoes(runtime);
		this.trackCommitted(sessionId, runtime);
		this.publishThread(sessionId);
	}

	/** Fold side-effects of the entry just applied (`fold.append` sets
	 * `fold.side`): steer strands, queued drain, error banners. */
	private applyFoldSide(sessionId: string, runtime: Runtime): void {
		const side = runtime.fold.side;
		if (side.turnFinished) {
			const stranded = new Set(side.turnFinished.strandedSteerIds);
			runtime.echoes = runtime.echoes.map((e) =>
				e.steerPendingId
					? {
						...e,
						steerFailed: stranded.has(e.steerPendingId) ? true : e.steerFailed,
						steerPendingId: null,
					}
					: e,
			);
		}
		if (side.turnStarted) {
			runtime.echoes = runtime.echoes.map((e) => (e.queued ? { ...e, queued: false } : e));
			if (runtime.turnStartedAt === null) runtime.turnStartedAt = Date.now();
		}
		if (side.threadError) {
			this.patch(
				updateThread(this.state, sessionId, (t) => ({ ...t, error: side.threadError })),
			);
		} else if (side.turnStarted) {
			this.patch(
				updateThread(this.state, sessionId, (t) => (t.error === null ? t : { ...t, error: null })),
			);
		}
	}

	/** `running` projection edge bookkeeping (turn duration). */
	private tickRunning(_sessionId: string, runtime: Runtime): void {
		const running = asBool(this.projectionValue(runtime, 'running')) === true;
		if (running && runtime.turnStartedAt === null) {
			runtime.turnStartedAt = Date.now();
		} else if (!running && runtime.turnStartedAt !== null) {
			runtime.lastTurnDurationSec = Math.max(
				0,
				Math.round((Date.now() - runtime.turnStartedAt) / 1000),
			);
			runtime.turnStartedAt = null;
		}
	}

	private publishThread(sessionId: string): void {
		const runtime = this.sessions.get(sessionId);
		if (!runtime) return;
		this.patch(
			updateThread(this.state, sessionId, (t) => this.deriveThread(sessionId, t, runtime)),
		);
	}

	/** Derive the observable thread state from the runtime (one source). */
	private deriveThread(_sessionId: string, t: ThreadState, runtime: Runtime): ThreadState {
		const items: TranscriptItem[] = [
			...runtime.fold.items,
			...(runtime.cards as TranscriptItem[]),
			...runtime.echoes.map(echoItem),
		];
		const model = modelRefOf(this.projectionValue(runtime, 'model'));
		const pendingAuth = this.projectionValue(runtime, 'pending_auth');
		return {
			...t,
			items,
			title: asString(this.projectionValue(runtime, 'title')) ?? t.title,
			cwd: asString(this.projectionValue(runtime, 'cwd')) ?? t.cwd,
			turnActive: asBool(this.projectionValue(runtime, 'running')) ?? t.turnActive,
			planMode: asBool(this.projectionValue(runtime, 'plan_mode')) ?? t.planMode,
			approvalMode: this.projectionsReady(runtime)
				? kebabMode(this.projectionValue(runtime, 'permission_mode'))
				: t.approvalMode,
			reasoningEffort: this.projectionsReady(runtime)
				? ((asString(this.projectionValue(runtime, 'reasoning_effort')) as ReasoningEffort | null) ??
					t.reasoningEffort)
				: t.reasoningEffort,
			modelRef: model ?? t.modelRef,
			branch: asString(this.projectionValue(runtime, 'branch')) ?? t.branch,
			plan: asPlan(this.projectionValue(runtime, 'plan')) ?? t.plan,
			goal: asGoal(this.projectionValue(runtime, 'goal')) ?? t.goal,
			pendingAuthIds:
				pendingAuth && isRecord(pendingAuth)
					? Object.keys(pendingAuth)
					: t.pendingAuthIds,
			backgroundTasks: mapRecord(runtime.fold.backgroundTasks),
			subagents: [...runtime.fold.subagents.values()].map((a) => ({
				id: a.id,
				agent_type: a.agentType,
				description: a.description,
				tool_uses: a.toolUses,
				latest_activity: a.latestActivity,
				status: a.status,
			})),
			subagentChildren: mapRecord(runtime.fold.subagentChildren),
			committed: runtime.committed,
			lastTurnDurationSec: runtime.lastTurnDurationSec,
		};
	}

	private projectionsReady(runtime: Runtime): boolean {
		return runtime.projections.size > 0;
	}

	// ── projections ───────────────────────────────────────────────────────

	private storeProjections(
		runtime: Runtime,
		values: Record<string, unknown>,
		asOfSeq: number,
	): void {
		for (const [key, value] of Object.entries(values)) {
			const prev = runtime.projections.get(key);
			// higher-seq-wins (equal seq keeps the newer frame — the frame
			// ordering the server guarantees within a stream).
			if (!prev || asOfSeq >= prev.seq) {
				runtime.projections.set(key, { value, seq: asOfSeq });
			}
		}
	}

	private projectionValue(runtime: Runtime, key: string): unknown {
		return runtime.projections.get(key)?.value;
	}

	private projectionValues(runtime: Runtime): Record<string, unknown> {
		const out: Record<string, unknown> = {};
		for (const [key, slot] of runtime.projections) out[key] = slot.value;
		return out;
	}

	// ── echo retirement + committed edge ──────────────────────────────────

	private retireEchoes(runtime: Runtime): void {
		let any = false;
		for (const row of runtime.rows) {
			if (row.originRpc !== null) {
				any = true;
				break;
			}
		}
		if (!any) return;
		const keys = new Set<string>();
		for (const row of runtime.rows) {
			if (row.originRpc !== null) keys.add(row.originRpc);
		}
		runtime.echoes = runtime.echoes.filter(
			(e) => e.originRpc === null || !keys.has(e.originRpc),
		);
	}

	/** Committed = durable user/assistant `message` rows inside the
	 * published window. The count advances exactly when the Q face can
	 * change (the §E.3 refresh edge signal); the pull itself belongs to the
	 * conversation-info plugin (T8 §H), which watches `committed` on the
	 * observable state and writes the payload back through
	 * `setConversationInfo`. */
	private trackCommitted(sessionId: string, runtime: Runtime): void {
		const cursors = runtime.engine.cursors();
		let count = 0;
		if (cursors.first !== undefined && cursors.last !== undefined) {
			for (const row of runtime.rows) {
				if (
					row.committed &&
					row.record.seq >= cursors.first &&
					row.record.seq <= cursors.last
				) {
					count += 1;
				}
			}
		}
		if (count !== runtime.committed) {
			runtime.committed = count;
			this.publishThread(sessionId);
		}
	}

	/** §E.2 read seam: the `{ value, seq }` slot of one projection key
	 * (frozen object — stable identity between changes, safe as a
	 * `useSyncExternalStore` source). `undefined` while unseen. */
	projection(sessionId: string, key: string): Readonly<ProjectionSlot> | undefined {
		const slot = this.sessions.get(sessionId)?.projections.get(key);
		return slot ? Object.freeze({ ...slot }) : undefined;
	}

	/** §E.3 write seam for the conversation-info plugin: park the folded
	 * payload on the thread state so every subscriber sees it through the
	 * store (never a component-local mirror). */
	setConversationInfo(sessionId: string, info: ConversationInfo | null): void {
		const current = this.state.perThread[sessionId];
		if (!current || current.conversationInfo === info) return;
		this.patch(
			updateThread(this.state, sessionId, (t) => ({ ...t, conversationInfo: info })),
		);
	}

	// ── history paging (§D.2 PageHistory) ─────────────────────────────────

	/** Prepend one backwards history page (the transcript's scroll-up
	 * affordance; the engine's `prependPage` data source). */
	async requestOlder(sessionId: string): Promise<void> {
		const runtime = this.sessions.get(sessionId);
		if (!runtime || runtime.paging) return;
		const head = runtime.engine.cursors().first;
		if (head === undefined || head <= 0) return;
		runtime.paging = true;
		try {
			const page = await this.effects.pageHistory(sessionId, head - 1);
			if (!page) return;
			for (const record of page.records) {
				insertRow(runtime.rows, {
					record,
					originRpc: originOf(record),
					committed: isCommittedRow(record),
				});
			}
			runtime.engine.prependPage(
				page.records.map((r) => ({ first: r.seq, last: r.seq })),
				page.hasMore,
			);
		} finally {
			runtime.paging = false;
		}
	}

	hasMoreHistory(sessionId: string): boolean {
		const runtime = this.sessions.get(sessionId);
		if (!runtime) return false;
		const head = runtime.engine.cursors().first;
		return head !== undefined && head > 0;
	}
}

// ── helpers ────────────────────────────────────────────────────────────────

/** Insert a row into the ascending-seq cache (dense seq ⇒ the tail fast
 * path covers the live stream). */
function insertRow(rows: WindowRecord[], row: WindowRecord): void {
	const last = rows[rows.length - 1];
	if (last === undefined || row.record.seq > last.record.seq) {
		rows.push(row);
		return;
	}
	if (last !== undefined && row.record.seq === last.record.seq) return; // idempotent
	for (let i = rows.length - 1; i >= 0; i -= 1) {
		const cur = rows[i];
		if (cur === undefined) continue;
		if (cur.record.seq === row.record.seq) return; // already cached
		if (cur.record.seq < row.record.seq) {
			rows.splice(i + 1, 0, row);
			return;
		}
	}
	rows.unshift(row);
}

function originOf(record: WireRecord): string | null {
	return record.type === 'message' && record.role === 'user'
		? asString(record.originRpc)
		: null;
}

function isCommittedRow(record: WireRecord): boolean {
	return record.type === 'message' && (record.role === 'user' || record.role === 'assistant');
}

function echoItem(echo: Echo): TranscriptItem {
	return {
		kind: 'user',
		id: echo.key,
			clientId: echo.key,
			originRpc: echo.originRpc,
		text: echo.text,
		timestamp: echo.timestamp,
		modelId: echo.modelRef,
		images: echo.images,
		queued: echo.queued,
		steerPendingId: echo.steerPendingId ?? null,
		steerFailed: echo.steerFailed,
	};
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

function updateThread(
	state: ChatState,
	sessionId: string,
	f: (t: ThreadState) => ThreadState,
): ChatState {
	const current = state.perThread[sessionId] ?? initThread(sessionId);
	const next = f(current);
	if (next === current && state.perThread[sessionId]) return state;
	return { ...state, perThread: { ...state.perThread, [sessionId]: next } };
}

function mapRecord<T>(map: Map<string, T>): Record<string, T> {
	const out: Record<string, T> = {};
	for (const [key, value] of map) out[key] = value;
	return out;
}

function asPlan(value: unknown): PlanSnapshotWire | null {
	if (!isRecord(value)) return null;
	if (!Array.isArray(value.steps)) return null;
	return value as unknown as PlanSnapshotWire;
}

function asGoal(value: unknown): GoalSnapshotWire | null {
	if (!isRecord(value)) return null;
	if (typeof value.objective !== 'string') return null;
	return value as unknown as GoalSnapshotWire;
}
