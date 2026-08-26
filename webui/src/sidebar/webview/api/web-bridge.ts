// Bridge over a WebSocket to the browser host (the in-app axum server):
// typed messages as JSON frames. On a dropped socket the connection is
// rebuilt with a fixed retry — the backend replays session state through
// idempotent open_thread, so a lost frame window self-heals on reconnect.
import type { HostToWebview, WebviewToHost } from '../../messages';
import type { Bridge } from './bridge';

export interface WebBridgeOptions {
  /** Shortest wait before a reconnect attempt, in milliseconds. */
  reconnectDelay?: number;
}

declare const __MANOX_TOKEN__: string | undefined;

const RECONNECT_DELAY = 500;

export function createWebBridge(options: WebBridgeOptions = {}): Bridge {
  const listeners = new Set<(message: HostToWebview) => void>();
  const reconnectDelay = options.reconnectDelay ?? RECONNECT_DELAY;
  let ws: WebSocket | null = null;
  let closed = false;

  const wsUrl = () => {
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    return `${scheme}://${location.host}/ws?token=${encodeURIComponent(__MANOX_TOKEN__ ?? '')}`;
  };

  const connect = () => {
    if (closed) return;
    ws = new WebSocket(wsUrl());
    ws.onmessage = (event) => {
      let message: HostToWebview;
      try {
        message = JSON.parse(event.data as string) as HostToWebview;
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
      if (ws?.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify(message));
      }
    },
    onMessage(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}
