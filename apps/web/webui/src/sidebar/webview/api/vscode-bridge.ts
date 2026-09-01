// Bridge over the VS Code webview API: postMessage out to the extension
// host, window messages in. Selected only when acquireVsCodeApi is present.
import type { HostToWebview, WebviewToHost } from '../../messages';
import type { Bridge } from './bridge';

interface VscodeHostApi {
  postMessage(msg: WebviewToHost): void;
}

declare function acquireVsCodeApi(): VscodeHostApi;

export function isVscodeHost(): boolean {
  return typeof acquireVsCodeApi === 'function';
}

export function createVscodeBridge(): Bridge {
  const host = acquireVsCodeApi();
  const listeners = new Set<(message: HostToWebview) => void>();

  window.addEventListener('message', (raw: MessageEvent) => {
    const message = raw.data as HostToWebview;
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
