// Bridge over the VS Code webview API: postMessage out to the extension
// host, window messages in. Selected only when acquireVsCodeApi is present.
// The host is a transparent typed relay: it forwards `FromClient` to the
// napi `AgentServer` connection and `FromServer` back here verbatim; the few
// host-only lifecycle verbs (`HostVerb`) are intercepted for orchestration.
import type { ToHost, ToWebview } from '../../messages';
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
	const listeners = new Set<(message: ToWebview) => void>();

	window.addEventListener('message', (raw: MessageEvent) => {
		const message = raw.data as ToWebview;
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
	};
}
