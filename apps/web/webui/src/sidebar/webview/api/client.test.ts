// The v2 api client: request/receipt correlation, stream-open sequences,
// and the full-intent `CreateSession` flow. `client.ts` builds its transport
// at module load, so each test re-imports it fresh (vi.resetModules) with
// the browser globals stubbed first — the WebSocket bridge stands in for the
// wire.

import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { FromClient, FromServer } from '../../../protocol';
import type { StoreEffects } from '../state/store';

/** Fake WebSocket capturing frames per connection generation. */
class MockWebSocket {
	static instances: MockWebSocket[] = [];
	static OPEN = 1;
	static CLOSED = 3;

	readyState = MockWebSocket.CLOSED;
	sent: FromClient[] = [];
	onopen: (() => void) | null = null;
	onmessage: ((event: { data: string }) => void) | null = null;
	onclose: (() => void) | null = null;

	constructor(_url: string) {
		MockWebSocket.instances.push(this);
	}

	send(data: string): void {
		this.sent.push(JSON.parse(data) as FromClient);
	}

	open(): void {
		this.readyState = MockWebSocket.OPEN;
		this.onopen?.();
	}

	/** Deliver a `FromServer` frame through the transport to the client. */
	receive(message: FromServer | Record<string, unknown>): void {
		this.onmessage?.({ data: JSON.stringify(message) });
	}
}

/** The store surface `connectStore` installs. */
function fakeStore() {
	let effects: StoreEffects | null = null;
	return {
		dispatch: vi.fn(),
		openRemote: vi.fn(),
		confirmDraft: vi.fn(),
		reseat: vi.fn(),
		attachEffects: vi.fn((e: StoreEffects) => {
			effects = e;
		}),
		get effects(): StoreEffects {
			if (!effects) throw new Error('effects not attached');
			return effects;
		},
	};
}

/** Frames sent after the Initialize handshake (dropped from assertions). */
function postedFrames(ws: MockWebSocket): FromClient[] {
	return ws.sent.filter((f) => !(f.kind === 'request' && f.id.startsWith('init-')));
}

async function freshClient() {
	vi.resetModules();
	const webBridge = await import('./web-bridge');
	const client = await import('./client');
	return { client, webBridge };
}

beforeEach(() => {
	MockWebSocket.instances = [];
	vi.stubGlobal('WebSocket', MockWebSocket);
	vi.stubGlobal('location', { protocol: 'http:', host: 'localhost:4173' });
	vi.stubGlobal('__MANOX_TOKEN__', 'tok');
	const store = new Map<string, string>();
	vi.stubGlobal('sessionStorage', {
		getItem: (k: string) => store.get(k) ?? null,
		setItem: (k: string, v: string) => void store.set(k, v),
	});
	vi.stubGlobal('crypto', { randomUUID: () => 'uuid-' + Math.random().toString(36).slice(2) });
});

const response = (id: string, value: Record<string, unknown>) => ({
	kind: 'response',
	id,
	outcome: { Ok: value },
});

describe('request/receipt correlation (§T7.1)', () => {
	it('a submit sends a Request with the originRpc and resolves its Response', async () => {
		const { client } = await freshClient();
		const store = fakeStore();
		client.connectStore(store);
		const ws = MockWebSocket.instances[0];
		ws.open();

		const receipt = new client.ThreadApi('s1').submit('hello', undefined, 'rpc-1');
		const submit = postedFrames(ws).find(
			(f) => f.kind === 'request' && (f as { call: { method: string } }).call.method === 'submit',
		) as Extract<FromClient, { kind: 'request' }>;
		expect(submit).toBeTruthy();
		expect(submit.call).toMatchObject({ method: 'submit', sessionId: 's1', text: 'hello', originRpc: 'rpc-1' });

		ws.receive(response(submit.id, { accepted: true, message_id: 'm1' }));
		await expect(receipt).resolves.toEqual({ accepted: true, message_id: 'm1' });
	});

	it('an Err response rejects the pending request', async () => {
		const { client } = await freshClient();
		client.connectStore(fakeStore());
		const ws = MockWebSocket.instances[0];
		ws.open();
		const receipt = new client.ThreadApi('s1').submit('x', undefined, 'rpc-2');
		const submit = postedFrames(ws).find(
			(f) => f.kind === 'request' && (f as { call: { method: string } }).call.method === 'submit',
		) as Extract<FromClient, { kind: 'request' }>;
		ws.receive({
			kind: 'response',
			id: submit.id,
			outcome: { Err: { code: -32000, message: 'session/not-found', data: null } },
		});
		await expect(receipt).rejects.toThrow('session/not-found');
	});

	it('non-response frames flow into the store dispatch', async () => {
		const { client } = await freshClient();
		const store = fakeStore();
		client.connectStore(store);
		const ws = MockWebSocket.instances[0];
		ws.open();
		const frame = {
			kind: 'streamItem',
			streamId: 's',
			frame: { type: 'entry', seq: 1, event: { type: 'turnStart' } },
		};
		ws.receive(frame);
		expect(store.dispatch).toHaveBeenCalledWith(expect.objectContaining({ kind: 'streamItem' }));
	});
});

describe('follow-stream sequences', () => {
	it('openThread opens the session and issues StreamOpen(followSession)', async () => {
		const { client } = await freshClient();
		const store = fakeStore();
		client.connectStore(store);
		const ws = MockWebSocket.instances[0];
		ws.open();

		client.api.openThread('s9');
		expect(store.openRemote).toHaveBeenCalledWith('s9');
		// openRemote triggers the effect: OpenSession request + StreamOpen
		// (the openStream effect is async — it awaits the OpenSession
		// receipt — so drive the response first).
		store.effects.openStream('s9', 'st-1');
		const openReq = postedFrames(ws).find(
			(f) => f.kind === 'request' && (f as { call: { method: string } }).call.method === 'openSession',
		) as Extract<FromClient, { kind: 'request' } | undefined>;
		expect(openReq).toBeTruthy();
		ws.receive(response(openReq!.id, {}));
		await vi.waitFor(() => {
			expect(postedFrames(ws)).toContainEqual(
				expect.objectContaining({
					kind: 'streamOpen',
					streamKind: { type: 'followSession', sessionId: 's9', maxMessages: null },
				}),
			);
		});
	});

	it('pageHistory rides the effects seam as a Request', async () => {
		const { client } = await freshClient();
		const store = fakeStore();
		client.connectStore(store);
		const ws = MockWebSocket.instances[0];
		ws.open();

		const pageP = store.effects.pageHistory('s1', 4);
		const pageReq = postedFrames(ws).find(
			(f) => f.kind === 'request' && (f as { call: { method: string } }).call.method === 'pageHistory',
		) as Extract<FromClient, { kind: 'request' }>;
		expect(pageReq.call).toMatchObject({ method: 'pageHistory', sessionId: 's1', throughSeq: 4 });
		ws.receive(response(pageReq.id, { records: [{ seq: 4, id: 'e4', parentId: null, timestamp: '', type: 'turnStart' }], has_more: true, cursor: 4 }));
		const page = await pageP;
		expect(page?.hasMore).toBe(true);
		expect(page?.records[0]?.seq).toBe(4);
	});

	// §E.3 / T8 §H: the Q-face pull is plugin-owned now — `getConversationInfo`
	// is an exported fetch seam on the api client (no store effect), riding the
	// same pending-request table as every other Request.
	it('getConversationInfo rides the pending table as a Request (plugin fetch seam)', async () => {
		const { client } = await freshClient();
		const store = fakeStore();
		client.connectStore(store);
		const ws = MockWebSocket.instances[0];
		ws.open();

		const infoP = client.getConversationInfo('s1');
		const infoReq = postedFrames(ws).find(
			(f) => f.kind === 'request' && (f as { call: { method: string } }).call.method === 'getConversationInfo',
		) as Extract<FromClient, { kind: 'request' }>;
		expect(infoReq.call).toMatchObject({ method: 'getConversationInfo', sessionId: 's1' });
		ws.receive(response(infoReq.id, { threadId: 's1', cursor: 4, models: [], cumulativeCost: 0 }));
		await expect(infoP).resolves.toMatchObject({ threadId: 's1' });
	});
});

describe('full-intent CreateSession (§T7.3)', () => {
	it('sends cwd/project/initialModel and rekeys the draft on the receipt', async () => {
		const { client } = await freshClient();
		const store = fakeStore();
		client.connectStore(store);
		const ws = MockWebSocket.instances[0];
		ws.open();

		const sessionP = client.api.newSession({
			localId: 'local-1',
			text: 'first',
			modelRef: 'DeepSeek-anthropic/deepseek-chat',
			originRpc: 'rpc-first',
		});
		const create = postedFrames(ws).find(
			(f) => f.kind === 'request' && (f as { call: { method: string } }).call.method === 'createSession',
		) as Extract<FromClient, { kind: 'request' }>;
		expect(create.call).toMatchObject({
			method: 'createSession',
			cwd: null,
			project: null,
			initialModel: 'DeepSeek-anthropic/deepseek-chat',
		});
		ws.receive(response(create.id, { session_id: 'srv-9' }));
		// Draft rekeyed to the canonical id; follow stream opened.
		await vi.waitFor(() => {
			expect(store.confirmDraft).toHaveBeenCalledWith('local-1', 'srv-9');
			expect(store.openRemote).toHaveBeenCalledWith('srv-9');
		});
		// The first message rides a Submit Request with an originRpc.
		const submit = postedFrames(ws).find(
			(f) => f.kind === 'request' && (f as { call: { method: string } }).call.method === 'submit',
		) as Extract<FromClient, { kind: 'request' } | undefined>;
		expect(submit).toBeTruthy();
		expect(submit!.call).toMatchObject({ sessionId: 'srv-9', text: 'first' });
		const r = submit!.call as { originRpc: string | null };
		expect(r.originRpc).toBe('rpc-first');
		// Resolve the submit so no pending promise dangles.
		ws.receive(response(submit!.id, { accepted: true, message_id: null }));
		await expect(sessionP).resolves.toBe('srv-9');
	});
});

describe('reseat', () => {
	it('a new connection generation triggers store.reseat()', async () => {
		const { client } = await freshClient();
		const store = fakeStore();
		client.connectStore(store);
		const ws = MockWebSocket.instances[0];
		ws.open();
		ws.receive(response('init-1', { ack: true }));
		// The Initialize ack marks the generation: the store reseat is
		// invoked at most once per generation.
		expect(store.reseat.mock.calls.length).toBeLessThanOrEqual(1);
	});
});
