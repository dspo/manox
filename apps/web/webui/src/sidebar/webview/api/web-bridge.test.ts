import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { createWebBridge } from './web-bridge';

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

describe('createWebBridge', () => {
  it('opens the same-origin /ws socket with the page token', () => {
    createWebBridge();
    expect(MockWebSocket.instances).toHaveLength(1);
    expect(MockWebSocket.instances[0].url).toBe(
      'ws://localhost:4173/ws?token=tok%2F1%202',
    );
  });

  it('posts typed messages as JSON frames once the socket is open', () => {
    const bridge = createWebBridge();
    const ws = MockWebSocket.instances[0];
    ws.open();
    bridge.post({ type: 'list_threads' });
    expect(ws.sent).toEqual([JSON.stringify({ type: 'list_threads' })]);
  });

  it('drops posts while the socket is not open', () => {
    const bridge = createWebBridge();
    const ws = MockWebSocket.instances[0];
    bridge.post({ type: 'list_threads' });
    expect(ws.sent).toEqual([]);
  });

  it('parses incoming JSON frames and dispatches to subscribers', () => {
    const bridge = createWebBridge();
    const listener = vi.fn();
    const unsubscribe = bridge.onMessage(listener);

    MockWebSocket.instances[0].receive({ type: 'session_disposed', sessionId: 's1' });
    expect(listener).toHaveBeenCalledWith({ type: 'session_disposed', sessionId: 's1' });

    unsubscribe();
    MockWebSocket.instances[0].receive({ type: 'ready' });
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
    MockWebSocket.instances[1].receive({ type: 'ready' });
    expect(listener).toHaveBeenCalledWith({ type: 'ready' });
  });
});
