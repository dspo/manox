// Bridge over a WebSocket to the browser host (the in-app axum server):
// typed `FromClient`/`FromServer` frames. On a dropped socket the connection
// is rebuilt with a fixed retry; the persisted `client_id` makes the new
// socket a re-seat of the same logical client (the server cancels the stale
// generation, §B/K.5), and the api client re-follows its streams on each
// established generation (`onConnection`).
//
// Inbound frames pass the hand-written §D.8 guards before being forwarded:
// a frame whose envelope kind is unknown, or whose stream/host payload fails
// the exact-key check, is dropped + logged — never forwarded, never
// disconnecting (L12 tolerance).
//
// The browser path owns its own `Initialize` handshake: the axum
// `WebSocketConnection` server expects it as the first frame and fails any
// other call until it lands. The client_id is minted once and persisted in
// sessionStorage so a page reload re-seats the same identity while the tab
// lives.
import type { FromClient, FromServer } from '../../../protocol';
import { parseHostEvent, parseStreamEndReason, parseStreamFrame } from '../../../../../../../crates/manox-protocol/bindings/guards';
import type { HostNote, ToHost } from '../../messages';
import type { Bridge } from './bridge';

export interface WebBridgeOptions {
	/** Shortest wait before a reconnect attempt, in milliseconds. */
	reconnectDelay?: number;
	/** Override the client-id store (tests). */
	clientIdStore?: Pick<Storage, 'getItem' | 'setItem'>;
}

declare const __MANOX_TOKEN__: string | undefined;

const RECONNECT_DELAY = 500;
const CLIENT_ID_KEY = 'manox.webui.client-id';

/** Capabilities the browser webview can answer (full hook surface). */
const WEB_CAPABILITIES = [
	'approve',
	'planVerdict',
	'askUserQuestion',
	'browserOp',
	'clipboardRead',
	'openExternal',
] as const;

const defaultClientIdStore = (): Pick<Storage, 'getItem' | 'setItem'> | null => {
	try {
		return typeof sessionStorage === 'undefined' ? null : sessionStorage;
	} catch {
		// Private-mode Safari throws on sessionStorage access.
		return null;
	}
};

const mintClientId = (): string => {
	// The persisted id must be stable across reconnects (re-seat identity);
	// `crypto.randomUUID` is the MsgId mint the whole webui shares.
	return `webui-${globalThis.crypto.randomUUID()}`;
};

const isRecord = (v: unknown): v is Record<string, unknown> =>
	typeof v === 'object' && v !== null && !Array.isArray(v);

/** §D.1 envelope validation: exact top-level shape per `kind` + the payload
 * guards for stream/host frames. Returns the typed frame or null (drop+log). */
export function parseFromServer(raw: unknown): FromServer | null {
	if (!isRecord(raw) || typeof raw.kind !== 'string') return null;
	switch (raw.kind) {
		case 'response':
			if (typeof raw.id !== 'string' || !isRecord(raw.outcome)) return null;
			return raw as unknown as FromServer;
		case 'request':
			if (typeof raw.id !== 'string' || !isRecord(raw.call)) return null;
			return raw as unknown as FromServer;
		case 'notification':
			if (!isRecord(raw.note) || typeof raw.note.method !== 'string') return null;
			return raw as unknown as FromServer;
		case 'host': {
			const guard = parseHostEvent(raw.host);
			if (!guard.ok) {
				console.warn('[webui] host frame dropped:', guard.reason);
				return null;
			}
			return { kind: 'host', host: guard.value } as unknown as FromServer;
		}
		case 'streamItem': {
			if (typeof raw.streamId !== 'string') return null;
			const guard = parseStreamFrame(raw.frame);
			if (!guard.ok) {
				console.warn('[webui] stream frame dropped:', guard.reason);
				return null;
			}
			return { kind: 'streamItem', streamId: raw.streamId, frame: guard.value } as unknown as FromServer;
		}
		case 'streamEnd': {
			if (typeof raw.streamId !== 'string') return null;
			const guard = parseStreamEndReason(raw.reason);
			if (!guard.ok) {
				console.warn('[webui] stream end dropped:', guard.reason);
				return null;
			}
			return { kind: 'streamEnd', streamId: raw.streamId, reason: guard.value } as unknown as FromServer;
		}
		default:
			console.warn('[webui] unknown FromServer envelope kind dropped', raw.kind);
			return null;
	}
}

export function createWebBridge(options: WebBridgeOptions = {}): Bridge {
	const listeners = new Set<(message: FromServer | HostNote) => void>();
	const connectionListeners = new Set<{ on: () => void; off: () => void }>();
	const reconnectDelay = options.reconnectDelay ?? RECONNECT_DELAY;
	const store = options.clientIdStore ?? defaultClientIdStore();
	let clientId = store?.getItem(CLIENT_ID_KEY) ?? '';
	if (!clientId) {
		clientId = mintClientId();
		store?.setItem(CLIENT_ID_KEY, clientId);
	}
	let ws: WebSocket | null = null;
	let closed = false;
	/** True while the current generation's Initialize is awaiting its ack. */
	let awaitingAck = false;
	let msgSeq = 0;

	const wsUrl = () => {
		const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
		return `${scheme}://${location.host}/ws?token=${encodeURIComponent(__MANOX_TOKEN__ ?? '')}`;
	};

	const send = (message: FromClient) => {
		if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(message));
	};

	const connect = () => {
		if (closed) return;
		ws = new WebSocket(wsUrl());
		ws.onopen = () => {
			// First frame on a fresh socket is always the Initialize
			// handshake, carrying the persisted client_id (re-seat, §B).
			awaitingAck = true;
			const init: FromClient = {
				kind: 'request',
				id: `init-${++msgSeq}`,
				call: {
					method: 'initialize',
					clientId,
					capabilities: [...WEB_CAPABILITIES],
					sessions: [],
				},
			};
			send(init);
		};
		ws.onmessage = (event) => {
			let raw: unknown;
			try {
				raw = JSON.parse(event.data as string);
			} catch {
				return;
			}
			if (!isRecord(raw)) return;
			// Host-only out-of-band UI note (never an agent wire frame).
			if (raw.kind === 'open_turn_navigator') {
				for (const listener of listeners) listener(raw as unknown as HostNote);
				return;
			}
			const message = parseFromServer(raw);
			if (message === null) return;
			if (message.kind === 'notification' && message.note.method === 'ready') {
				// The axum pump answers Initialize with a `Response` ack and
				// then the v1 `Ready` note; either counts as the generation
				// being established.
				awaitingAck = false;
				for (const listener of connectionListeners) listener.on();
			}
			if (message.kind === 'response' && typeof message.id === 'string' && message.id.startsWith('init-')) {
				if (awaitingAck) {
					awaitingAck = false;
					for (const listener of connectionListeners) listener.on();
				}
			}
			for (const listener of listeners) listener(message);
		};
		ws.onclose = () => {
			ws = null;
			awaitingAck = false;
			for (const listener of connectionListeners) listener.off();
			setTimeout(connect, reconnectDelay);
		};
	};

	connect();

	return {
		post(message) {
			// Host-only verbs never reach the browser host; the webview is
			// expected to translate them into `FromClient` sequences before
			// posting (see `client.ts`). A stray verb is dropped.
			if (
				isRecord(message) &&
				typeof message.kind === 'string' &&
				({ new_session: 1, open_thread: 1, plan_execute_fresh: 1 } as Record<string, number>)[
					message.kind
				] !== undefined
			) {
				return;
			}
			send(message as FromClient);
		},
		onMessage(listener) {
			listeners.add(listener);
			return () => listeners.delete(listener);
		},
		onConnection(on, off) {
			const entry = { on, off };
			connectionListeners.add(entry);
			return () => connectionListeners.delete(entry);
		},
	};
}
