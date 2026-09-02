import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { createWebBridge } from './web-bridge';
import type { FromClient, FromServer } from '../../../protocol';

/** Fake WebSocket standing in for the browser transport in a node test env. */
class MockWebSocket {
  static instances: MockWebSocket[] = [];
  static OPEN = 1;
  static CLOSED = 3;

  readyState = MockWebSocket.CLOSED;
  url: string;
  sent: string[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;

  constructor(url: string) {
    this.url = url;
    MockWebSocket.instances.push(this);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  open(): void {
    this.readyState = MockWebSocket.OPEN;
    this.onopen?.();
  }

  receive(payload: unknown): void {
    this.onmessage?.({ data: JSON.stringify(payload) });
  }

  drop(): void {
    this.readyState = MockWebSocket.CLOSED;
    this.onclose?.();
  }
}

beforeEach(() => {
  MockWebSocket.instances = [];
  vi.stubGlobal('WebSocket', MockWebSocket);
  vi.stubGlobal('location', { protocol: 'http:', host: 'localhost:4173' });
  vi.stubGlobal('__MANOX_TOKEN__', 'tok/1 2');
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

/** A representative `FromClient` notification used to exercise posting. */
const listThreads: FromClient = { kind: 'request', id: 'r1', call: { method: 'listThreads' } };

describe('createWebBridge', () => {
  it('opens the same-origin /ws socket with the page token', () => {
    createWebBridge();
    expect(MockWebSocket.instances).toHaveLength(1);
    expect(MockWebSocket.instances[0].url).toBe(
      'ws://localhost:4173/ws?token=tok%2F1%202',
    );
  });

  it('sends the Initialize handshake as the first frame on open', () => {
    createWebBridge();
    const ws = MockWebSocket.instances[0];
    ws.open();
    expect(ws.sent).toHaveLength(1);
    const init = JSON.parse(ws.sent[0]) as FromClient;
    expect(init.kind).toBe('request');
    expect((init as { call: unknown }).call).toMatchObject({ method: 'initialize' });
  });

  it('posts typed FromClient frames as JSON once the socket is open', () => {
    const bridge = createWebBridge();
    const ws = MockWebSocket.instances[0];
    ws.open();
    ws.sent.length = 0; // drop the Initialize frame
    bridge.post(listThreads);
    expect(ws.sent).toEqual([JSON.stringify(listThreads)]);
  });

  it('drops posts while the socket is not open', () => {
    const bridge = createWebBridge();
    const ws = MockWebSocket.instances[0];
    bridge.post(listThreads);
    expect(ws.sent).toEqual([]);
  });

  it('parses incoming FromServer frames and dispatches to subscribers', () => {
    const bridge = createWebBridge();
    const listener = vi.fn();
    const unsubscribe = bridge.onMessage(listener);

    const note: FromServer = {
      kind: 'notification',
      note: { method: 'sessionDisposed', sessionId: 's1' },
    };
    MockWebSocket.instances[0].receive(note);
    expect(listener).toHaveBeenCalledWith(note);

    unsubscribe();
    MockWebSocket.instances[0].receive({ kind: 'notification', note: { method: 'ready' } });
    expect(listener).toHaveBeenCalledTimes(1);
  });

  it('reconnects after the drop and keeps delivering', () => {
    const bridge = createWebBridge();
    const first = MockWebSocket.instances[0];
    first.drop();
    expect(MockWebSocket.instances).toHaveLength(1);

    vi.advanceTimersByTime(500);
    expect(MockWebSocket.instances).toHaveLength(2);

    const listener = vi.fn();
    bridge.onMessage(listener);
    MockWebSocket.instances[1].receive({ kind: 'notification', note: { method: 'ready' } });
    expect(listener).toHaveBeenCalledWith({ kind: 'notification', note: { method: 'ready' } });
  });
});
