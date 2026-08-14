// Typed bridge to the extension host: postMessage out, typed HostToWebview
// payloads in. All actor traffic arrives wrapped in HostToWebview `event`.

import type { HostToWebview, WebviewToHost } from '../../messages';

interface HostApi {
  postMessage(msg: WebviewToHost): void;
}

declare function acquireVsCodeApi(): HostApi;

const host = acquireVsCodeApi();

export const api = {
  submit(text: string): void {
    host.postMessage({ type: 'submit', text });
  },
  approve(id: string, allow: boolean): void {
    host.postMessage({ type: 'approve', id, allow });
  },
  cancel(): void {
    host.postMessage({ type: 'cancel' });
  },
  setModel(id: string): void {
    host.postMessage({ type: 'set_model', id });
  },
  requestModels(): void {
    host.postMessage({ type: 'request_models' });
  },
  requestUsage(): void {
    host.postMessage({ type: 'request_usage' });
  },
  newSession(): void {
    host.postMessage({ type: 'new_session' });
  },
};

export function onHostMessage(handler: (msg: HostToWebview) => void): void {
  window.addEventListener('message', (e: MessageEvent<HostToWebview>) => handler(e.data));
}
