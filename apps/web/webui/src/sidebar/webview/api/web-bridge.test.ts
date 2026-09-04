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

// ── T7 additions: frame guards, receipts, persisted client_id, generations ─

describe('createWebBridge (v2 frame routing)', () => {
	/** A storage-backed sessionStorage stand-in. */
	function stubSessionStorage(): Map<string, string> {
		const map = new Map<string, string>();
		vi.stubGlobal('sessionStorage', {
			getItem: (k: string) => map.get(k) ?? null,
			setItem: (k: string, v: string) => map.set(k, v),
		} as unknown as Storage);
		return map;
	}

	const initOk = {
		kind: 'response',
		id: 'init-1',
		outcome: { Ok: { ack: true } },
	};

	it('forwards Response frames to listeners (receipts are no longer dropped)', () => {
		stubSessionStorage();
		const bridge = createWebBridge();
		const listener = vi.fn();
		bridge.onMessage(listener);
		MockWebSocket.instances[0].receive(initOk);
		expect(listener).toHaveBeenCalledWith(expect.objectContaining({ kind: 'response' }));
	});

	it('drops unknown envelope kinds, malformed stream frames, and unknown host tags', () => {
		stubSessionStorage();
		const bridge = createWebBridge();
		const listener = vi.fn();
		bridge.onMessage(listener);
		MockWebSocket.instances[0].receive({ kind: 'what-is-this' });
		MockWebSocket.instances[0].receive({
			kind: 'streamItem',
			streamId: 's',
			frame: { type: 'entry', seq: 1, event: { type: 'turnStart' }, extraKey: true },
		});
		MockWebSocket.instances[0].receive({ kind: 'host', host: { type: 'notAHostTag' } });
		expect(listener).not.toHaveBeenCalled();
	});

	it('forwards valid stream + host frames with guarded payloads', () => {
		stubSessionStorage();
		const bridge = createWebBridge();
		const listener = vi.fn();
		bridge.onMessage(listener);
		MockWebSocket.instances[0].receive({
			kind: 'streamItem',
			streamId: 's1',
			frame: { type: 'entry', seq: 1, event: { type: 'turnStart' } },
		});
		MockWebSocket.instances[0].receive({
			kind: 'host',
			host: {
				type: 'sessionStatus',
				sessionId: 's1',
				running: true,
				errored: null,
				unread: null,
				pendingAuth: null,
				pendingPlan: null,
				backgroundWork: null,
			},
		});
		expect(listener).toHaveBeenCalledTimes(2);
	});

	it('mints a stable client_id in sessionStorage and reuses it across reconnects', () => {
		const store = stubSessionStorage();
		const bridge = createWebBridge();
		bridge.onMessage(() => undefined);
		const first = MockWebSocket.instances[0];
		first.open();
		const init = JSON.parse(first.sent[0]) as FromClient;
		const clientId = (init as { call: { clientId: string } }).call.clientId;
		expect(clientId).toMatch(/^webui-/);
		expect(store.get('manox.webui.client-id')).toBe(clientId);
		first.drop();
		vi.advanceTimersByTime(500);
		const second = MockWebSocket.instances[1];
		second.open();
		const init2 = JSON.parse(second.sent[0]) as FromClient;
		expect((init2 as { call: { clientId: string } }).call.clientId).toBe(clientId);
	});

	it('fires onConnection once per generation (init ack), onDisconnect on drop', () => {
		stubSessionStorage();
		const bridge = createWebBridge();
		const on = vi.fn();
		const off = vi.fn();
		bridge.onConnection?.(on, off);
		const first = MockWebSocket.instances[0];
		first.open();
		// Ack before Ready.
		first.receive(initOk);
		expect(on).toHaveBeenCalledTimes(1);
		first.drop();
		expect(off).toHaveBeenCalledTimes(1);
		vi.advanceTimersByTime(500);
		const second = MockWebSocket.instances[1];
		second.open();
		// The v1 `ready` note also marks the generation established.
		second.receive({ kind: 'notification', note: { method: 'ready' } });
		expect(on).toHaveBeenCalledTimes(2);
	});
});
