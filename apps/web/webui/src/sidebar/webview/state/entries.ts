// Journal-entry → transcript fold (T7, §F.2): the single place that turns
// `JournalWireEntry` records (§C.2 vocabulary) into `TranscriptItem`s, for
// both the snapshot window (`replace`) and the live tail (`append`). This is
// a *generic UI fold* over the window — not domain state: every value is read
// off the record itself (L6). No other module interprets journal payloads.
//
// Streaming reconciliation: `agentTextDelta` / `agentThinkingDelta` entries
// grow the trailing draft bubble; the durable `message{role:assistant}` row
// that lands at turn settle *finalizes* it (the trailing draft is replaced
// with the authoritative text) instead of stacking a duplicate. Tool lifecycle
// keys on `callId` (`toolCall` / `toolOutputChunk` / `toolResult` durable
// entries plus the `approval` decision rows).

import { isKnownJournalTag } from '../../../../../../../crates/manox-protocol/bindings/guards';
import type { BackgroundTaskSnapshotWire } from '../../../protocol';
import type { SubagentChildWire } from '../../../protocol';
import type { ToolCallState, TranscriptItem, UserImage } from './transcript';
import { foldToolStatus } from './transcript';

/** One §C.1 envelope row: `JournalWireEntry` (the snapshot `records` shape)
 * or the `{seq, event}` pair of a `StreamFrame::Entry` frame, normalized to
 * the flattened envelope form. Guards have already checked `type` against
 * the declared vocabulary (L12); field extraction below is tolerant so wire
 * drift degrades to "ignored", never an exception. */
export interface WireRecord {
	readonly seq: number;
	readonly id: string;
	readonly timestamp: string;
	readonly type: string;
	readonly [key: string]: unknown;
}

/** The per-turn transcript facts the fold derives (not projections): the
 * running flag's transcript mirror (queued-chip drain), the steer-strand
 * markers, and the model ref currently in force (canonical `{provider}/
 * {modelId}`, tracked from `modelChange` rows for per-item display stamps —
 * display only, never identity resolution, L6). */
export interface FoldSideEffects {
	turnStarted: boolean;
	turnFinished: { failed: boolean; strandedSteerIds: string[] } | null;
	stop: boolean;
	threadError: string | null;
}

const asString = (v: unknown): string | null => (typeof v === 'string' ? v : null);
const asNumber = (v: unknown): number | null =>
	typeof v === 'number' && Number.isFinite(v) ? v : null;
const asBool = (v: unknown): boolean => v === true;
const asRecord = (v: unknown): Record<string, unknown> | null =>
	typeof v === 'object' && v !== null && !Array.isArray(v)
		? (v as Record<string, unknown>)
		: null;
const asArray = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);

const capOutputTail = (text: string): string => {
	const CAP = 64_000;
	if (text.length <= CAP) return text;
	let start = text.length - CAP;
	const code = text.charCodeAt(start);
	if (code >= 0xdc00 && code <= 0xdfff) start += 1;
	return text.slice(start);
};

/** Content blocks of a `message` row, kernel storage shape (L12-tolerant:
 * unknown block shapes are skipped, never thrown). */
interface BlockText { type: 'text'; text: string }
interface BlockThinking { type: 'thinking'; text: string; redacted?: boolean }
interface BlockImage { type: 'image'; data: string; mimeType: string }
interface BlockToolUse { type: 'toolCall'; id: string; name: string; arguments: unknown }

const parseBlock = (raw: unknown):
	| BlockText
	| BlockThinking
	| BlockImage
	| BlockToolUse
	| null => {
	const b = asRecord(raw);
	if (!b) return null;
	switch (asString(b.type)) {
		case 'text':
			return { type: 'text', text: asString(b.text) ?? '' };
		case 'thinking':
			return {
				type: 'thinking',
				text: asString(b.thinking) ?? '',
				redacted: typeof b.redacted === 'boolean' ? b.redacted : undefined,
			};
		case 'image':
			return {
				type: 'image',
				data: asString(b.data) ?? '',
				mimeType: asString(b.mimeType) ?? 'application/octet-stream',
			};
		case 'toolCall':
			return {
				type: 'toolCall',
				id: asString(b.id) ?? '',
				name: asString(b.name) ?? '',
				arguments: b.arguments,
			};
		default:
			return null;
	}
};

/** `backgroundTask` snapshot wire shape (camelCase keys; the kernel's
 * `TaskSnapshot` serialized whole). */
const parseBackgroundTask = (raw: unknown): BackgroundTaskSnapshotWire | null => {
	const s = asRecord(raw);
	if (!s) return null;
	const taskId = asString(s.taskId) ?? asString(s.task_id);
	if (!taskId) return null;
	return {
		task_id: taskId,
		kind: (asString(s.kind) ?? 'BackgroundBash') as BackgroundTaskSnapshotWire['kind'],
		owner_thread_id: asString(s.ownerThreadId) ?? '',
		description: asString(s.description) ?? '',
		status: (asString(s.status) ?? 'Running') as BackgroundTaskSnapshotWire['status'],
		created_at_ms: asNumber(s.createdAtMs) ?? asNumber(s.created_at_ms) ?? 0,
		ended_at_ms: asNumber(s.endedAtMs) ?? asNumber(s.ended_at_ms) ?? null,
		event_count: asNumber(s.eventCount) ?? 0,
		total_bytes: asNumber(s.totalBytes) ?? 0,
		exit_code: asNumber(s.exitCode) ?? null,
		failure_summary: asString(s.failureSummary) ?? null,
		output_tail: asString(s.outputTail) ?? undefined,
	};
};

const secondsOf = (iso: string): number | null => {
	const ms = Date.parse(iso);
	return Number.isNaN(ms) ? null : Math.round(ms / 1000);
};

let foldCounter = 0;
const nextFoldId = (prefix: string): string => `${prefix}-${++foldCounter}`;

/** Mutable incremental fold over the journal window (see module header).
 * `modelRef` is the canonical ref in force during the fold (seeded from the
 * snapshot's `model` projection by the store, advanced by `modelChange`
 * rows); `autoApproved` accumulates the approval-decision ids whose tool card
 * had not landed yet (§C.2 approval dual-state fold). */
export class TranscriptFold {
	items: TranscriptItem[] = [];
	private readonly toolIndex = new Map<string, number>();
	readonly autoApproved = new Set<string>();
	private pendingAutoApprovals: string[] = [];
	/** Canonical `{provider}/{modelId}` in force; stamps new assistant items
	 * (display only). */
	modelRef: string | null = null;
	private lastAssistantSeq = -1;
	private lastThinkingSeq = -1;
	/** The `subagentProgress` rows folded so far (agents list source). */
	readonly subagents = new Map<
		string,
		{ id: string; agentType: string; description: string; toolUses: number; latestActivity: string | null; status: string }
	>();
	/** Streamed child-session events per sub-agent (mini-panel source). */
	readonly subagentChildren = new Map<string, SubagentChildWire[]>();
	/** Live background-task snapshots keyed by task id. */
	readonly backgroundTasks = new Map<string, BackgroundTaskSnapshotWire>();

	side: FoldSideEffects = emptySide();

	/** Re-fold the whole window (Replace / Prepend). */
	replace(records: readonly WireRecord[]): void {
		this.reset();
		for (const record of records) this.append(record);
	}

	/** Apply one live `Entry` frame (fresh seq only; the engine guarantees
	 * monotonicity, and re-applied rows are dropped upstream). */
	append(record: WireRecord): void {
		this.side = emptySide();
		this.applyRecord(record);
	}

	/** Fold a snapshot's projection baseline into the transcript's structural
	 * rows (goal/plan/background tasks ride projections, §E.2; the plan/goal
	 * *values* stay in the projection map — only their transcript-visible
	 * artifacts fold here). */
	seedProjections(values: Record<string, unknown>): void {
		const tasks = values.background_tasks;
		if (tasks && typeof tasks === 'object' && !Array.isArray(tasks)) {
			for (const raw of Object.values(tasks as Record<string, unknown>)) {
				const task = parseBackgroundTask(raw);
				if (!task) continue;
				if (!this.backgroundTasks.has(task.task_id)) {
					this.backgroundTasks.set(task.task_id, task);
					this.items.push({ kind: 'background_task', id: `bg-${task.task_id}`, task });
				}
			}
		}
	}

	private reset(): void {
		this.items = [];
		this.toolIndex.clear();
		this.autoApproved.clear();
		this.pendingAutoApprovals = [];
		this.subagents.clear();
		this.subagentChildren.clear();
		this.backgroundTasks.clear();
		this.lastAssistantSeq = -1;
		this.lastThinkingSeq = -1;
		this.side = emptySide();
	}

	private applyRecord(r: WireRecord): void {
		switch (r.type) {
			case 'message':
				this.onMessage(r);
				return;
			case 'uiNote':
				this.onUiNote(r);
				return;
			case 'turnStart':
				this.side.turnStarted = true;
				return;
			case 'turnFinish':
				this.side.turnFinished = {
					failed: asBool(r.failed),
					strandedSteerIds: asArray(r.strandedSteerIds).filter(
						(v): v is string => typeof v === 'string',
					),
				};
				return;
			case 'stop':
				this.side.stop = true;
				return;
			case 'error':
				this.side.threadError = asString(r.message);
				return;
			case 'agentTextDelta': {
				const s = asString(r.s);
				if (s === null) return;
				const last = this.items[this.items.length - 1];
				if (last && last.kind === 'assistant' && this.lastAssistantSeq === r.seq - 1) {
					this.items[this.items.length - 1] = { ...last, text: last.text + s };
				} else {
					this.items.push({
						kind: 'assistant',
						id: nextFoldId('assistant'),
						text: s,
						modelId: this.modelRef,
					});
				}
				this.lastAssistantSeq = r.seq;
				return;
			}
			case 'agentThinkingDelta': {
				const s = asString(r.s);
				if (s === null) return;
				const last = this.items[this.items.length - 1];
				if (last && last.kind === 'thinking' && this.lastThinkingSeq === r.seq - 1) {
					this.items[this.items.length - 1] = { ...last, text: last.text + s };
				} else {
					this.items.push({ kind: 'thinking', id: nextFoldId('thinking'), text: s });
				}
				this.lastThinkingSeq = r.seq;
				return;
			}
			case 'toolCall': {
				const callId = asString(r.callId);
				if (!callId) return;
				const name = asString(r.name) ?? '';
				this.upsertTool(callId, (prev) => {
					const status = foldToolStatus(asString(r.status) ?? 'running');
					return {
						id: callId,
						name,
						title: asString(r.title) || prev?.title || name || callId,
						status,
						output: prev?.output ?? '',
						isError: status === 'failed' ? true : (prev?.isError ?? false),
						autoApproved: prev?.autoApproved || this.drainAutoApproval(callId) || undefined,
					};
				});
				return;
			}
			case 'toolResult': {
				const callId = asString(r.callId);
				if (!callId) return;
				const isError = asBool(r.isError);
				this.upsertTool(callId, (prev) => ({
					id: callId,
					name: prev?.name ?? '',
					title: prev?.title ?? callId,
					status: isError ? 'failed' : prev && isTerminal(prev.status) ? prev.status : 'completed',
					output: capOutputTail(asString(r.output) ?? ''),
					isError,
					autoApproved: prev?.autoApproved || this.drainAutoApproval(callId) || undefined,
				}));
				return;
			}
			case 'toolOutputChunk': {
				const callId = asString(r.callId);
				const chunk = asString(r.chunk);
				if (!callId || chunk === null) return;
				this.upsertTool(callId, (prev) => ({
					id: callId,
					name: prev?.name ?? '',
					title: prev?.title ?? callId,
					status: prev?.status ?? 'running',
					output: capOutputTail((prev?.output ?? '') + chunk),
					isError: prev?.isError ?? false,
					autoApproved: prev?.autoApproved,
				}));
				return;
			}
			case 'subagentChild': {
				const agentId = asString(r.agentId);
				if (!agentId) return;
				const prior = this.subagentChildren.get(agentId) ?? [];
				this.subagentChildren.set(
					agentId,
					[...prior, r.event as SubagentChildWire].slice(-200),
				);
				return;
			}
			case 'subagentProgress': {
				const id = asString(r.agentId);
				if (!id) return;
				this.subagents.set(id, {
					id,
					agentType: asString(r.agentType) ?? this.subagents.get(id)?.agentType ?? '',
					description: this.subagents.get(id)?.description ?? '',
					toolUses: asNumber(r.toolUses) ?? 0,
					latestActivity: asString(r.latestActivity),
					status: asString(r.status) ?? 'running',
				});
				return;
			}
			case 'modelChange': {
				const to = asString(r.to);
				if (to) this.modelRef = to;
				return;
			}
			case 'backgroundTask': {
				const task = parseBackgroundTask(r.snapshot);
				if (!task) return;
				const known = this.backgroundTasks.has(task.task_id);
				this.backgroundTasks.set(task.task_id, task);
				if (!known) {
					this.items.push({ kind: 'background_task', id: `bg-${task.task_id}`, task });
				}
				return;
			}
			case 'approval': {
				const kind = asString(r.kind);
				const authId = asString(r.authId);
				if (!authId) return;
				// Requests ride the waterfall ServerCall (the adjudication
				// card source); only the decision's `allow` verdict folds
				// (the auto-approve badge). The pending set itself is the
				// `pending_auth` projection (P face).
				if (kind === 'decision' && asString(r.verdict) === 'allow') {
					const callId = asString(r.toolCallId);
					if (callId) this.markAutoApproved(callId);
				}
				return;
			}
			case 'compactionStarted':
				this.items.push({ kind: 'compaction', id: nextFoldId('compaction-started'), summary: '' });
				return;
			case 'compaction':
				// The durable row finalizes the started spinner's summary text.
				this.onCompaction(r);
				return;
			default:
				// Transcript-irrelevant vocabulary (lifecycle edges, tree
				// bookkeeping, metrics) folds in the store's projection /
				// side-effect arms, not here.
				return;
		}
	}

	private onMessage(r: WireRecord): void {
		const role = asString(r.role);
		const blocks = asArray(r.content).map(parseBlock).filter((b): b is NonNullable<typeof b> => b !== null);
		if (role === 'user') {
			let text = '';
			const images: UserImage[] = [];
			for (const b of blocks) {
				if (b.type === 'text') text += b.text;
				if (b.type === 'image') {
					images.push({
						mimeType: b.mimeType,
						data: b.data ? `data:${b.mimeType};base64,${b.data}` : null,
						byteLen: b.data ? Math.ceil((b.data.length * 3) / 4) : null,
					});
				}
			}
			if (!text && images.length === 0) return;
			this.items.push({
				kind: 'user',
				id: r.id || nextFoldId('user'),
				text,
				timestamp: secondsOf(r.timestamp),
				images: images.length ? images : undefined,
			});
			return;
		}
		if (role === 'assistant') {
			this.onAssistantMessage(blocks, r);
			return;
		}
		if (role === 'tool') {
			this.onToolMessage(r);
		}
	}

	private onAssistantMessage(blocks: ReturnType<typeof parseBlock>[], r: WireRecord): void {
		// Durable assistant row: finalize the streamed draft(s) and stamp the
		// model in force. Thinking-first, then text — mirror the block order.
		let sawText = false;
		let sawThinking = false;
		for (const b of blocks) {
			if (!b) continue;
			if (b.type === 'thinking' && b.text.trim() && !b.redacted) {
				const last = this.items[this.items.length - 1];
				if (last && last.kind === 'thinking' && this.lastThinkingSeq !== -1) {
					this.items[this.items.length - 1] = { ...last, text: b.text };
				} else {
					this.items.push({ kind: 'thinking', id: nextFoldId('thinking'), text: b.text });
				}
				this.lastThinkingSeq = r.seq;
				sawThinking = true;
			}
			if (b.type === 'text' && b.text.trim()) {
				const last = this.items[this.items.length - 1];
				if (last && last.kind === 'assistant' && this.lastAssistantSeq !== -1) {
					// Replace the streamed draft with the authoritative text.
					this.items[this.items.length - 1] = { ...last, text: b.text };
				} else {
					this.items.push({
						kind: 'assistant',
						id: nextFoldId('assistant'),
						text: b.text,
						modelId: this.modelRef,
					});
				}
				this.lastAssistantSeq = r.seq;
				sawText = true;
			}
			if (b.type === 'toolCall' && b.id) {
				this.upsertTool(b.id, (prev) => ({
					id: b.id,
					name: prev?.name ?? b.name,
					title: prev?.title || `${b.name}(${JSON.stringify(b.arguments ?? {})})`,
					status: prev?.status ?? 'completed',
					output: prev?.output ?? '',
					isError: prev?.isError ?? false,
					autoApproved: prev?.autoApproved || this.drainAutoApproval(b.id) || undefined,
				}));
			}
		}
		// A streaming turn always ends with an authoritative row; reset the
		// draft continuations so a later delta opens a new bubble.
		if (sawText || sawThinking) {
			this.lastAssistantSeq = sawText ? r.seq : -1;
			this.lastThinkingSeq = sawThinking ? r.seq : -1;
		}
	}

	private onToolMessage(r: WireRecord): void {
		// `tool`-role rows carry the whole kernel message JSON (§C.2):
		// `toolResult` / `bashExecution` shapes.
		const msg = asRecord(asArray(r.content)[0]);
		if (!msg) return;
		const kind = asString(msg.role);
		if (kind === 'toolResult') {
			const callId = asString(msg.toolCallId);
			if (!callId) return;
			const isError = asBool(msg.isError);
			const text = asArray(msg.content)
				.map(parseBlock)
				.filter((b): b is BlockText => !!b && b.type === 'text')
				.map((b) => b.text)
				.join('');
			this.upsertTool(callId, (prev) => ({
				id: callId,
				name: prev?.name || asString(msg.toolName) || 'tool',
				title: prev?.title || asString(msg.toolName) || callId,
				status: isError ? 'failed' : prev && isTerminal(prev.status) ? prev.status : 'completed',
				output: capOutputTail(text),
				isError,
				autoApproved: prev?.autoApproved || this.drainAutoApproval(callId) || undefined,
			}));
			return;
		}
		if (kind === 'bashExecution') {
			const command = asString(msg.command) ?? '';
			const exitCode = asNumber(msg.exitCode);
			this.items.push({
				kind: 'tool',
				id: r.id,
				tool: {
					id: r.id,
					name: 'bash',
					title: command,
					status: asBool(msg.cancelled) ? 'cancelled' : exitCode === 0 ? 'completed' : 'failed',
					output: capOutputTail(asString(msg.output) ?? ''),
					isError: exitCode !== null && exitCode !== 0,
				},
			});
		}
	}

	private onUiNote(r: WireRecord): void {
		// §C.2 durable UI annotation: only the `error` kind has a client
		// surface today (the thread error banner); other kinds are display
		// notes the webui has no renderer for — ignore, never throw (L12).
		if ((asString(r.kind) ?? 'notice') !== 'error') return;
		const data = asRecord(r.data);
		this.side.threadError = data ? asString(data.text) ?? '' : '';
	}

	private onCompaction(r: WireRecord): void {
		const summary = asString(r.summary) ?? '';
		for (let i = this.items.length - 1; i >= 0; i -= 1) {
			const item = this.items[i];
			if (item && item.kind === 'compaction' && item.summary === '') {
				this.items[i] = { ...item, summary };
				return;
			}
		}
		this.items.push({ kind: 'compaction', id: nextFoldId('compaction'), summary });
	}

	private upsertTool(id: string, f: (prev: ToolCallState | undefined) => ToolCallState): void {
		const index = this.toolIndex.get(id);
		if (index === undefined) {
			this.toolIndex.set(id, this.items.length);
			this.items.push({ kind: 'tool', id, tool: f(undefined) });
			return;
		}
		const item = this.items[index];
		if (item && item.kind === 'tool') {
			this.items[index] = { kind: 'tool', id, tool: f(item.tool) };
		}
	}

	private markAutoApproved(callId: string): void {
		this.autoApproved.add(callId);
		const index = this.toolIndex.get(callId);
		if (index !== undefined) {
			const item = this.items[index];
			if (item && item.kind === 'tool' && !item.tool.autoApproved) {
				this.items[index] = { ...item, tool: { ...item.tool, autoApproved: true } };
			}
		} else if (!this.pendingAutoApprovals.includes(callId)) {
			this.pendingAutoApprovals.push(callId);
		}
	}

	/** True once when the id's parked auto-approval is consumed by a fresh
	 * tool card (the documented decision-before-call ordering race, §C.2). */
	private drainAutoApproval(callId: string): boolean {
		if (!this.pendingAutoApprovals.includes(callId)) return false;
		this.pendingAutoApprovals = this.pendingAutoApprovals.filter((x) => x !== callId);
		return true;
	}
}

const TERMINAL = new Set(['completed', 'failed', 'denied', 'cancelled', 'continued']);
const isTerminal = (status: string): boolean => TERMINAL.has(status);

const emptySide = (): FoldSideEffects => ({
	turnStarted: false,
	turnFinished: null,
	stop: false,
	threadError: null,
});

/** Normalize raw `PageHistory` rows / snapshot `records` JSON into the
 * flattened §C.1 envelope shape the fold consumes. Rows whose `type` is not
 * in the declared vocabulary, or whose seq is not a finite number, are
 * dropped (guards enforce tag membership at the boundary; this is the last
 * tolerant filter — L12). */
export const normalizeWireRecords = (raw: unknown): WireRecord[] => {
	if (!Array.isArray(raw)) return [];
	const out: WireRecord[] = [];
	for (const value of raw) {
		if (typeof value !== 'object' || value === null || Array.isArray(value)) continue;
		const row = value as Record<string, unknown>;
		const seq = asNumber(row.seq);
		const type = asString(row.type);
		if (seq === null || type === null || !isKnownJournalTag(type)) {
			console.warn('[webui] journal record dropped', type);
			continue;
		}
		out.push({
			...row,
			seq,
			type,
			id: asString(row.id) ?? `e-${seq}`,
			timestamp: asString(row.timestamp) ?? '',
		});
	}
	out.sort((a, b) => a.seq - b.seq);
	return out;
};
