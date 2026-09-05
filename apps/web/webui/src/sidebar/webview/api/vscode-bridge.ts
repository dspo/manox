// Bridge over the VS Code webview API: postMessage out to the extension
// host, window messages in. Selected only when acquireVsCodeApi is present.
// The host is a transparent typed relay: it forwards `FromClient` to the
// napi `AgentServer` connection and `FromServer` back here verbatim; the few
// host-only lifecycle verbs (`HostVerb`) are intercepted for orchestration.
//
// T7: the extension host owns the napi connection (and, from T9 on, the v2
// frame guards), so the webview sees one stable connection generation —
// `onConnection` is a no-op subscription (no reseat loop on this transport).
import type { FromServer } from '../../../protocol';
import type { HostNote, ToHost } from '../../messages';
import type { Bridge } from './bridge';

interface VscodeHostApi {
	postMessage(msg: ToHost): void;
}

declare function acquireVsCodeApi(): VscodeHostApi;

export function isVscodeHost(): boolean {
	return typeof acquireVsCodeApi === 'function';
}

export function createVscodeBridge(): Bridge {
	const host = acquireVsCodeApi();
	const listeners = new Set<(message: FromServer | HostNote) => void>();

	window.addEventListener('message', (raw: MessageEvent) => {
		const message = raw.data as FromServer | HostNote;
		for (const listener of listeners) listener(message);
	});

	return {
		post(message) {
			host.postMessage(message);
		},
		onMessage(listener) {
			listeners.add(listener);
			return () => listeners.delete(listener);
		},
		onConnection() {
			// The extension relay is the connection owner; the webview's
			// postMessage channel is always live once the panel exists.
			return () => undefined;
		},
	};
}
