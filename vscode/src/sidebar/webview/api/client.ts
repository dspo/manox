// Typed bridge to the extension host: postMessage out, typed HostToWebview
// payloads in. Every per-thread call carries its sessionId so the store can
// address any live thread; view switching stays inside the webview and never
// crosses this boundary.

import type { ApprovalMode, ImageAttachment } from '../../../protocol';
import type { HostToWebview, WebviewToHost } from '../../messages';

interface HostApi {
  postMessage(msg: WebviewToHost): void;
}

declare function acquireVsCodeApi(): HostApi;

const host = acquireVsCodeApi();

const listeners = new Set<(message: HostToWebview) => void>();

window.addEventListener('message', (raw: MessageEvent) => {
  const message = raw.data as HostToWebview;
  for (const listener of listeners) listener(message);
});

/** Subscribe to host messages; returns an unsubscribe function. */
export function onHostMessage(listener: (message: HostToWebview) => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function post(message: WebviewToHost): void {
  host.postMessage(message);
}

/** Per-thread command surface; one instance per live thread id. */
export class ThreadApi {
  constructor(readonly sessionId: string) {}

  submit(text: string, images?: ImageAttachment[]): void {
    post({ type: 'submit', sessionId: this.sessionId, text, images });
  }

  approve(id: string, allow: boolean): void {
    post({ type: 'approve', sessionId: this.sessionId, id, allow });
  }

  cancel(): void {
    post({ type: 'cancel', sessionId: this.sessionId });
  }

  setModel(id: string): void {
    post({ type: 'set_model', sessionId: this.sessionId, id });
  }

  setApprovalMode(mode: ApprovalMode): void {
    post({ type: 'set_approval_mode', sessionId: this.sessionId, mode });
  }

  requestUsage(): void {
    post({ type: 'request_usage', sessionId: this.sessionId });
  }

  requestThreadInfo(): void {
    post({ type: 'request_thread_info', sessionId: this.sessionId });
  }

  focus(): void {
    post({ type: 'focus_thread', sessionId: this.sessionId });
  }
}

/** Global command surface (thread registry, models, slash entries). */
export const api = {
  requestModels(): void {
    post({ type: 'request_models' });
  },
  newSession(): void {
    post({ type: 'new_session' });
  },
  listThreads(): void {
    post({ type: 'list_threads' });
  },
  openThread(sessionId: string): void {
    post({ type: 'open_thread', sessionId });
  },
  /** Clear the focused thread (leaving the conversation view) so turns that
   * finish afterwards mark it unread. */
  blurThread(): void {
    post({ type: 'focus_thread' });
  },
  listCommands(): void {
    post({ type: 'list_commands' });
  },
};
