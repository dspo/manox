// Typed bridge to the host: `FromClient` out (notifications, fire-and-forget
// list requests, and `Reply` verdicts to `ServerCall`s), `FromServer` in.
// The store is driven entirely by push delivery — list-type calls
// (`listThreads` / `listModels` / `listCommands`) send a `Request` whose
// `Response` the client ignores; the matching `ThreadsUpdated` / `Models` /
// `Commands` notification folds the result into state. No request/response
// correlation is needed on the webview side.
//
// Session lifecycle (new / open / plan-execute-fresh) is host-dependent: the
// VS Code extension intercepts it (its `SessionManager` owns the napi
// connection and per-session event routing); the browser host has no such
// orchestrator, so the webview posts the equivalent `FromClient` sequence
// directly. The host-verb `HostVerb` type carries the VS Code-only lifecycle.

import type {
	ApprovalMode,
	ClientCall,
	ClientNote,
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

// Pick the host transport at runtime: VS Code injects acquireVsCodeApi as a
// global, the browser host has only a WebSocket.
const bridge: Bridge = isVscodeHost() ? createVscodeBridge() : createWebBridge();

const navigatorListeners = new Set<() => void>();
let storeDispatch: ((msg: FromServer) => void) | null = null;
let storeOpenRemote: ((sessionId: string) => void) | null = null;

bridge.onMessage((message: ToWebview) => {
	// Host-only UI toggle (macOS cmd+m) never reaches the agent wire.
	if (
		typeof message === 'object' &&
		'kind' in message &&
		(message as { kind: string }).kind === 'open_turn_navigator'
	) {
		for (const listener of navigatorListeners) listener();
		return;
	}
	// `Response` frames are replies to fire-and-forget list requests the
	// store ignores (push delivery supersedes them) — drop them so the store
	// only ever folds `Notification` / `Request` (ServerCall) frames.
	const fs = message as FromServer;
	if (fs.kind === 'response') return;
	storeDispatch?.(fs);
});

/** Subscribe to host-requested turn-navigator toggles (macOS cmd+m, where
 * the OS minimize accelerator would swallow the key before the DOM). */
export function onOpenTurnNavigator(listener: () => void): () => void {
	navigatorListeners.add(listener);
	return () => navigatorListeners.delete(listener);
}

/** Wire the store's dispatch entry point. The bridge folds `Notification`
 * and `Request` (ServerCall) frames into state; `Response` frames are
 * dropped above. */
export function connectStore(store: {
	dispatch: (msg: FromServer) => void;
	openRemote: (sessionId: string) => void;
}): void {
	storeDispatch = store.dispatch.bind(store);
	storeOpenRemote = store.openRemote.bind(store);
}

let msgSeq = 0;
const nextId = (): MsgId => `webui-${++msgSeq}`;

function post(msg: FromClient): void {
	bridge.post(msg);
}

function postNote(note: ClientNote): void {
	post({ kind: 'notification', note });
}

function postRequest(call: ClientCall): void {
	post({ kind: 'request', id: nextId(), call });
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

/** Per-thread command surface; one instance per live thread id. */
export class ThreadApi {
	constructor(readonly sessionId: string) {}

	submit(text: string, images?: ImageAttachment[], clientId?: string): void {
		postNote({
			method: 'submit',
			sessionId: this.sessionId,
			text,
			images: images ?? [],
			clientId: clientId ?? null,
		});
	}

	/** Turn the queued message identified by `clientId` into a steer of the
	 * running turn; the server replies with `steerPending`. */
	steer(clientId: string, text: string, images?: ImageAttachment[]): void {
		postNote({
			method: 'steer',
			sessionId: this.sessionId,
			clientId,
			text,
			images: images ?? [],
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
	setModel(id: string): void {
		postNote({ method: 'setModel', sessionId: this.sessionId, id });
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
	 * VS Code orchestrates host-side; the browser posts the `FromClient`
	 * sequence directly (archive + create + seed). */
	planExecuteFresh(planFile: string, cwd: string): void {
		if (isVscodeHost()) {
			postVerb({ kind: 'plan_execute_fresh', sessionId: this.sessionId, planFile, cwd });
			return;
		}
		const freshId = globalThis.crypto.randomUUID();
		postNote({ method: 'archiveThread', sessionId: this.sessionId, archived: true });
		postNote({ method: 'createSession', sessionId: freshId, cwd: null });
		postNote({ method: 'planSeedExecution', sessionId: freshId, planFile });
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

	requestUsage(): void {
		postRequest({ method: 'getUsage', sessionId: this.sessionId });
	}

	requestThreadInfo(): void {
		postRequest({ method: 'threadInfo', sessionId: this.sessionId });
	}

	focus(): void {
		postNote({ method: 'focusThread', sessionId: this.sessionId });
	}
}

/** Global command surface (thread registry, models, slash entries). */
export const api = {
	requestModels(): void {
		postRequest({ method: 'listModels' });
	},
	/** Optional payload = home-composer first message: the caller picks the id
	 * so an optimistic draft can render before the session exists. */
	newSession(opts: {
		sessionId?: string;
		text?: string;
		images?: ImageAttachment[];
		modelId?: string;
	}): void {
		if (isVscodeHost()) {
			postVerb({ kind: 'new_session', ...opts });
			return;
		}
		// Browser: post the `FromClient` sequence the VS Code host would have
		// orchestrated. The caller has already drafted the thread locally.
		const sessionId = opts.sessionId ?? globalThis.crypto.randomUUID();
		postNote({ method: 'createSession', sessionId, cwd: null });
		if (opts.modelId) postNote({ method: 'setModel', sessionId, id: opts.modelId });
		if (opts.text || opts.images?.length) {
			postNote({
				method: 'submit',
				sessionId,
				text: opts.text ?? '',
				images: opts.images ?? [],
				clientId: null,
			});
		}
	},
	listThreads(): void {
		postRequest({ method: 'listThreads' });
	},
	listCommands(): void {
		postRequest({ method: 'listCommands' });
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
		// Browser: the server replays history via notifications on
		// `OpenSession`; switch the view optimistically (the store's
		// `SessionCreated` fold confirms it) and let `ThreadHistory` settle.
		storeOpenRemote?.(sessionId);
		postRequest({ method: 'openSession', sessionId });
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
