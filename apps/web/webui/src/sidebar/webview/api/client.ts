// Typed bridge to the host: `FromClient` out, `FromServer` in.
//
// T7 v2 frame routing (spec §T7.1):
// - `Response` frames are resolved against a per-`MsgId` pending table
//   (request/receipt correlation, §D.1/§L7). No frames are dropped for
//   being responses any more.
// - `StreamItem` / `StreamEnd` / `Host` frames are forwarded to the store,
//   which routes them by `stream_id` into the per-session `JournalStream`
//   engine (T3) and by tag into the host-event mirror.
// - The inbound parser (`parseFromServer`, web-bridge.ts) runs the §D.8
//   runtime guards; unknown envelope kinds and exact-key violations are
//   dropped + logged, never forwarded (L12 tolerance).
//
// Session lifecycle (new / open / plan-execute-fresh) is host-dependent: the
// VS Code extension intercepts it (its `SessionManager` owns the napi
// connection and per-session event routing); the browser host has no such
// orchestrator, so the webview posts the equivalent v2 `FromClient` sequence
// directly. The host-verb `HostVerb` type carries the VS Code-only lifecycle.
//
// WS `client_id` persistence (re-seat identity across reconnects, §B) lives
// in the transport layer (web-bridge.ts).

import type {
	ApprovalMode,
	ClientCall,
	ClientNote,
	ConversationInfo,
	FromClient,
	FromServer,
	GoalAction,
	HookKind,
	ImageAttachment,
	MsgId,
	PlanVerdictChoice,
	ReasoningEffort,
} from '../../../protocol';
import type { HostVerb, ToWebview } from '../../messages';
import type { Bridge } from './bridge';
import { createVscodeBridge, isVscodeHost } from './vscode-bridge';
import { createWebBridge } from './web-bridge';
import { normalizeWireRecords } from '../state/entries';
import type { JournalPageData, StoreEffects } from '../state/store';

// Pick the host transport at runtime: VS Code injects acquireVsCodeApi as a
// global, the browser host has only a WebSocket.
const bridge: Bridge = isVscodeHost() ? createVscodeBridge() : createWebBridge();

const navigatorListeners = new Set<() => void>();

/** The store surface the api layer drives (structurally typed so tests can
 * inject fakes). `reseat` restarts every engine + re-follows streams;
 * `confirmDraft` rekeys a client-minted draft id to the server's. */
interface StoreSink {
	dispatch(msg: FromServer): void;
	openRemote(sessionId: string): void;
	confirmDraft(localId: string, serverId: string): void;
	reseat(): void;
	attachEffects(effects: StoreEffects): void;
}
let storeSink: StoreSink | null = null;

// ── per-`MsgId` pending-request table (§D.1, §L7) ─────────────────────────
// `Response` frames carry the `MsgId` the request was sent under; the
// resolver consumes it (or rejects on `Err`). The table survives reconnects
// only through the store's reseat path (which re-follows streams rather than
// re-sending one-shot requests); one-shot requests whose response was lost
// across a drop are rejected — the caller retries with a fresh `MsgId`.
interface Pending {
	resolve(value: Record<string, unknown> | null): void;
	reject(reason: string): void;
}
const pending = new Map<MsgId, Pending>();

function request(call: ClientCall): Promise<Record<string, unknown> | null> {
	return new Promise((resolve, reject) => {
		const id = nextId();
		pending.set(id, { resolve, reject });
		post({ kind: 'request', id, call });
	});
}

function settlePending(id: MsgId, frame: Extract<FromServer, { kind: 'response' }>): void {
	const handler = pending.get(id);
	if (!handler) return; // orphan: already settled by a reseat or never sent
	pending.delete(id);
	if ('Ok' in frame.outcome) handler.resolve((frame.outcome.Ok ?? {}) as Record<string, unknown>);
	else handler.reject(frame.outcome.Err.message);
}

bridge.onMessage((message: ToWebview) => {
	// Host-only UI toggle (macOS cmd+m) never reaches the agent wire.
	if ('kind' in message && (message as { kind: string }).kind === 'open_turn_navigator') {
		for (const listener of navigatorListeners) listener();
		return;
	}
	const frame = message as FromServer;
	// Resolve the pending-request table for `Response` frames (§T7.1).
	if (frame.kind === 'response') {
		settlePending(frame.id, frame);
		return;
	}
	storeSink?.dispatch(frame);
});

// Connection lifecycle (T7 store reseat + transport generation).
bridge.onConnection?.(
	() => {
		// A fresh generation: restart every engine and re-follow streams
		// (the transport's `client_id` is persisted in sessionStorage so the
		// server treats this as a re-seat, not a new client). Fires once per
		// generation on the Initialize ack (web-bridge).
		storeSink?.reseat();
	},
	() => {
		// Link dropped: the transport retries on its own; no state change is
		// needed (the store's engines keep their last window published until
		// the next snapshot arrives).
	},
);

/** Subscribe to host-requested turn-navigator toggles (macOS cmd+m, where
 * the OS minimize accelerator would swallow the key before the DOM). */
export function onOpenTurnNavigator(listener: () => void): () => void {
	navigatorListeners.add(listener);
	return () => navigatorListeners.delete(listener);
}

/** Wire the store: install the transport effects seam (the api layer owns
 * the outbound calls the store makes — `openStream` / `pageHistory`) and
 * become the frame sink. The §E.3 Q-face pull lives with its consumer: the
 * conversation-info plugin calls {@link getConversationInfo} on committed
 * edges (T8 §H). */
export function connectStore(store: StoreSink): void {
	storeSink = store;
	store.attachEffects({
		openStream(sessionId, streamId) {
			void openFollowStream(sessionId, streamId);
		},
		async pageHistory(sessionId, throughSeq): Promise<JournalPageData | null> {
			// ts-rs types the seq fields `bigint`, but they travel as JSON
			// numbers (well below 2^53); `BigInt` would break `JSON.stringify`.
			const receipt = await request({
				method: 'pageHistory',
				sessionId,
				throughSeq: throughSeq as unknown as bigint,
				beforeSeq: null,
				maxMessages: null,
			});
			if (!receipt) return null;
			return {
				records: normalizeWireRecords(receipt.records),
				hasMore: receipt.has_more === true,
			};
		},
	});
}

/**
 * §E.3 Q-face fetch seam (L5 extension surface, §H): a plugin calls this to
 * run a `GetConversationInfo` fold. The request rides the same pending table
 * as everything else — zero notes, zero emits (L7: the response carries fold
 * output, not domain state; the durable data itself keeps arriving through
 * the journal stream).
 */
export async function getConversationInfo(
	sessionId: string,
): Promise<ConversationInfo | null> {
	const receipt = await request({ method: 'getConversationInfo', sessionId });
	if (!receipt) return null;
	return receipt as unknown as ConversationInfo;
}

let msgSeq = 0;
const nextId = (): MsgId => `webui-${Date.now().toString(36)}-${++msgSeq}`;

/** A fresh id to correlate an optimistic echo with its durable origin (§F.2). */
export const mintRpcId = (): string => globalThis.crypto.randomUUID();

function post(msg: FromClient): void {
	bridge.post(msg);
}

function postNote(note: ClientNote): void {
	post({ kind: 'notification', note });
}

/** Post a `Reply` to a `ServerCall`. `id` is the request id the server used
 * (Approve/AskUserQuestion echo the `authId`; PlanVerdict uses the session
 * id; other capability calls echo the server-minted id). */
function postReply(id: MsgId, ok: Record<string, unknown>): void {
	post({ kind: 'reply', id, outcome: { Ok: ok as never } });
}

function postVerb(verb: HostVerb): void {
	bridge.post(verb);
}

/** Open a follow stream for `sessionId` under `streamId` (§D.1). `OpenSession`
 * first (receipt only — ownership; the follow pump requires the session to
 * materialize, §D.2), then `StreamOpen`. */
async function openFollowStream(sessionId: string, streamId: string): Promise<void> {
	try {
		await request({ method: 'openSession', sessionId });
	} catch {
		// Unknown/reclaimed session: the stream request would fail the same
		// way; let the `StreamEnd{Failure}` path surface it.
	}
	post({
		kind: 'streamOpen',
		streamId,
		streamKind: { type: 'followSession', sessionId, maxMessages: null },
	});
}

/** Per-thread command surface; one instance per live thread id. */
export class ThreadApi {
	constructor(readonly sessionId: string) {}

	/** §D.2 `Submit`: a Request carrying the caller's echo-retirement
	 * `originRpc`; the receipt resolves `{accepted, message_id?}`. */
	async submit(
		text: string,
		images?: ImageAttachment[],
		originRpc?: string,
	): Promise<Record<string, unknown> | null> {
		return request({
			method: 'submit',
			sessionId: this.sessionId,
			text,
			images: images ?? [],
			originRpc: originRpc ?? null,
		});
	}

	/** Turn the queued message identified by `originRpc` into a steer of the
	 * running turn. */
	async steer(originRpc: string, text: string, images?: ImageAttachment[]): Promise<void> {
		await request({
			method: 'steer',
			sessionId: this.sessionId,
			messageId: originRpc,
			text,
			images: images ?? [],
			originRpc: null,
		});
	}

	/** Drop a queued message (its echo bubble is removed locally too). */
	dropQueued(clientId: string): void {
		postNote({ method: 'dropQueued', sessionId: this.sessionId, clientId });
	}

	/** Resolve a `ServerCall::Approve` — reply `{ allow }` (the server's
	 * deterministic id for Approve is the `authId` the card carries). */
	approve(authId: string, allow: boolean): void {
		postReply(authId, { allow });
	}

	/** Resolve an `AskUserQuestion` card: per-question selections (labels
	 * joined by ", ") plus an optional free-form supplemental note. */
	answerQuestion(authId: string, answers: [string, string][], response: string | null): void {
		postReply(authId, { answers, response });
	}

	cancel(): void {
		postNote({ method: 'cancelTurn', sessionId: this.sessionId });
	}
	/** §D.3 / L8: the picker sends the canonical `{provider}/{model}` ref. */
	setModel(ref: string): void {
		postNote({ method: 'setModel', sessionId: this.sessionId, id: ref });
	}

	setReasoningEffort(effort: ReasoningEffort): void {
		postNote({ method: 'setReasoningEffort', sessionId: this.sessionId, effort });
	}

	setApprovalMode(mode: ApprovalMode): void {
		postNote({ method: 'setApprovalMode', sessionId: this.sessionId, mode });
	}

	setPlanMode(enabled: boolean): void {
		postNote({ method: 'setPlanMode', sessionId: this.sessionId, enabled });
	}

	/** Resolve a `ServerCall::PlanVerdict` (the server's deterministic id is
	 * the session id). */
	planVerdict(choice: PlanVerdictChoice): void {
		postReply(this.sessionId, { choice });
	}

	/** Execute-fresh: archive this session and seed a new one with the plan.
	 * VS Code orchestrates host-side; the browser posts the v2 `FromClient`
	 * sequence directly (archive + create + seed). */
	planExecuteFresh(planFile: string, cwd: string): void {
		if (isVscodeHost()) {
			postVerb({ kind: 'plan_execute_fresh', sessionId: this.sessionId, planFile, cwd });
			return;
		}
		postNote({ method: 'archiveThread', sessionId: this.sessionId, archived: true });
		void request({
			method: 'createSession',
			cwd,
			project: null,
			initialModel: null,
			approvalMode: null,
			reasoningEffort: null,
		})
			.then((receipt) => {
				const created = (receipt?.session_id as string | undefined) ?? this.sessionId;
				storeSink?.openRemote(created);
				postNote({ method: 'planSeedExecution', sessionId: created, planFile });
			})
			.catch(() => undefined);
	}

	goal(action: GoalAction, objective?: string, budget?: number): void {
		postNote({
			method: 'goal',
			sessionId: this.sessionId,
			action,
			objective: objective ?? null,
			budget: budget != null ? BigInt(budget) : null,
			maxRounds: null,
		});
	}

	stopBackgroundTask(taskId: string): void {
		postNote({ method: 'stopBackgroundTask', sessionId: this.sessionId, taskId });
	}

	focus(): void {
		postNote({ method: 'focusThread', sessionId: this.sessionId });
	}
}

/** Global command surface (thread registry, models, slash entries). */
export const api = {
	requestModels(): void {
		void request({ method: 'listModels' }).catch(() => undefined);
	},
	/** §D.2 `CreateSession`: full intent (§T7.3) — cwd/project/initial_model
	 * ride the Request; the receipt carries the canonical session id; the
	 * transcript arrives via the follow stream. `originRpc` correlates the
	 * caller's optimistic draft echo. */
	async newSession(opts: {
		/** Client-minted draft id (the caller drafted the thread locally). */
		localId?: string;
		text?: string;
		images?: ImageAttachment[];
		/** Canonical `{provider}/{modelId}` (L8). */
		modelRef?: string | null;
		cwd?: string | null;
		originRpc?: string;
	}): Promise<string | null> {
		if (isVscodeHost()) {
			postVerb({
				kind: 'new_session',
				sessionId: opts.localId,
				text: opts.text,
				images: opts.images,
				modelId: opts.modelRef ?? undefined,
			});
			return opts.localId ?? null;
		}
		// Browser: post the v2 `FromClient` sequence the VS Code host would
		// have orchestrated. The local draft is rekeyed to the server's
		// canonical id when the receipt lands.
		const localId = opts.localId ?? globalThis.crypto.randomUUID();
		const receipt = await request({
			method: 'createSession',
			cwd: opts.cwd ?? null,
			project: null,
			initialModel: opts.modelRef ?? null,
			approvalMode: null,
			reasoningEffort: null,
		});
		const sessionId = (receipt?.session_id as string | undefined) ?? localId;
		storeSink?.confirmDraft(localId, sessionId);
		storeSink?.openRemote(sessionId);
		// Draft-mode first message rides on the new session's stream; the
		// echo (tagged with `originRpc`) retires when the durable user entry
		// lands (§F.2).
		if (opts.text || opts.images?.length) {
			await new ThreadApi(sessionId).submit(opts.text ?? '', opts.images ?? [], opts.originRpc);
		}
		return sessionId;
	},
	listThreads(): void {
		void request({ method: 'listThreads' }).catch(() => undefined);
	},
	listCommands(): void {
		void request({ method: 'listCommands' }).catch(() => undefined);
	},
	archiveThread(sessionId: string, archived: boolean): void {
		postNote({ method: 'archiveThread', sessionId, archived });
	},
	pinThread(sessionId: string, pinned: boolean): void {
		postNote({ method: 'pinThread', sessionId, pinned });
	},
	openThread(sessionId: string): void {
		if (isVscodeHost()) {
			postVerb({ kind: 'open_thread', sessionId });
			return;
		}
		// Browser: switch the view optimistically (the store's follow stream
		// confirms it); the `OpenSession` receipt is just ownership (§D.2).
		storeSink?.openRemote(sessionId);
	},
	/** Clear the focused thread (leaving the conversation view) so turns that
	 * finish afterwards mark it unread. */
	blurThread(): void {
		postNote({ method: 'focusThread', sessionId: null });
	},
};

/** Re-export for component ergonomics (the host capability set the webview
 * declares). */
export type { HookKind };
