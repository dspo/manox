// Bridge over a WebSocket to the browser host (the in-app axum server):
// typed `FromClient`/`FromServer` frames. On a dropped socket the connection
// is rebuilt with a fixed retry — the server replays session state through
// idempotent `OpenSession`, so a lost frame window self-heals on reconnect.
//
// The browser path owns its own `Initialize` handshake (client_id
// `webui-{random}`): the axum `WebSocketConnection` server expects it as the
// first frame and fails any other call until it lands.
import type { FromClient, FromServer, HookKind } from '../../../protocol';
import type { ToHost, ToWebview } from '../../messages';
import type { Bridge } from './bridge';

export interface WebBridgeOptions {
	/** Shortest wait before a reconnect attempt, in milliseconds. */
	reconnectDelay?: number;
}

declare const __MANOX_TOKEN__: string | undefined;

const RECONNECT_DELAY = 500;

/** Capabilities the browser webview can answer (full hook surface). */
const WEB_CAPABILITIES: HookKind[] = [
	'approve',
	'planVerdict',
	'askUserQuestion',
	'browserOp',
	'clipboardRead',
	'openExternal',
];

export function createWebBridge(options: WebBridgeOptions = {}): Bridge {
	const listeners = new Set<(message: ToWebview) => void>();
	const reconnectDelay = options.reconnectDelay ?? RECONNECT_DELAY;
	let ws: WebSocket | null = null;
	let closed = false;
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
			// First frame on a fresh socket is always the Initialize handshake.
			const clientId = `webui-${Math.random().toString(36).slice(2, 10)}`;
			const init: FromClient = {
				kind: 'request',
				id: `init-${++msgSeq}`,
				call: {
					method: 'initialize',
					clientId,
					capabilities: WEB_CAPABILITIES,
					sessions: [],
				},
			};
			send(init);
		};
		ws.onmessage = (event) => {
			let message: FromServer;
			try {
				message = JSON.parse(event.data as string) as FromServer;
			} catch {
				return;
			}
			for (const listener of listeners) listener(message);
		};
		ws.onclose = () => {
			ws = null;
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
				'kind' in message &&
				(message as { kind: string }).kind in
					({ new_session: 1, open_thread: 1, plan_execute_fresh: 1 } as Record<string, number>)
			) {
				return;
			}
			send(message as FromClient);
		},
		onMessage(listener) {
			listeners.add(listener);
			return () => listeners.delete(listener);
		},
	};
}
