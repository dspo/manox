// Store v2 behaviour tests (T7). The store is driven purely by `dispatch`
// of v2 `FromServer` frames — no timers, no transport. Fixtures from
// `crates/manox-protocol/fixtures/` exercise the guards + fold path; the
// hand-built sequences exercise engine/echo/projection/host semantics.

import { describe, expect, it, vi } from 'vitest';

import type { FromServer } from '../../../protocol';
import snapshotFrame from '../../../../../../../crates/manox-protocol/fixtures/frames-snapshot.json';
import journalEntries from '../../../../../../../crates/manox-protocol/fixtures/journal-entries.json';
import hostEvents from '../../../../../../../crates/manox-protocol/fixtures/host-events.json';
import { Store, type JournalPageData, type StoreEffects } from './store';
import type { ConversationInfo } from '../../../protocol';
import type { WireRecord } from './entries';
import type { TranscriptItem } from './transcript';

const SESSION = 's1';

/** A snapshot frame carrying `records` (dense 0-based) — the wire `cursor`
 * is the entry count (§D.1), which the store translates to the engine's
 * inclusive tail. */
const snapshot = (records: unknown[], extra: Record<string, unknown> = {}) => ({
	type: 'snapshot',
	sessionId: SESSION,
	header: { id: SESSION, cwd: '/proj', parentSession: null, metadata: null, createdAt: '' },
	cursor: records.length === 0 ? 0 : (records[records.length - 1] as { seq: number }).seq + 1,
	records,
	hasMore: false,
	projections: {},
	projectionsAsOfSeq: 0,
	...extra,
});

const host = (event: unknown): FromServer => ({ kind: 'host', host: event }) as FromServer;

const note = (n: Record<string, unknown>): FromServer =>
	({ kind: 'notification', note: n }) as unknown as FromServer;

const callReq = (id: string, c: Record<string, unknown>): FromServer =>
	({ kind: 'request', id, call: c }) as unknown as FromServer;

/** A store with a pre-opened follow stream (the effect allocates the id). */
function openStore(): {
	store: Store;
	effects: ReturnType<typeof spyEffects>;
	/** Wrap a frame as a `StreamItem` on the session's stream. */
	item(frame: unknown): FromServer;
	/** Wrap a reason as a `StreamEnd` on the session's stream. */
	end(reason: unknown): FromServer;
} {
	const store = new Store();
	const effects = spyEffects();
	store.attachEffects(effects);
	store.openRemote(SESSION);
	expect(effects.openStream).toHaveBeenCalledWith(SESSION, expect.any(String));
	const streamId = effects.openStream.mock.calls[0]![1] as string;
	return {
		store,
		effects,
		item: (frame: unknown) => ({ kind: 'streamItem', streamId, frame }) as unknown as FromServer,
		end: (reason: unknown) => ({ kind: 'streamEnd', streamId, reason }) as unknown as FromServer,
	};
}

function spyEffects() {
	return {
		openStream: vi.fn(),
		pageHistory: vi.fn(async (): Promise<JournalPageData | null> => null),
		conversationInfo: vi.fn(async (): Promise<ConversationInfo | null> => null),
	};
}

const items = (store: Store): TranscriptItem[] => store.get().perThread[SESSION]?.items ?? [];

const thread = (store: Store) => store.get().perThread[SESSION];

/** Minimal §C.1 envelope row. */
const entryRow = (seq: number, event: Record<string, unknown>): Record<string, unknown> => ({
	seq,
	id: `e-${seq}`,
	parentId: seq === 0 ? null : `e-${seq - 1}`,
	timestamp: '2026-09-04T00:00:00Z',
	...event,
});

const userMsg = (seq: number, text: string, originRpc: string | null = null) =>
	entryRow(seq, { type: 'message', role: 'user', content: [{ type: 'text', text }], usage: null, originRpc });

const assistantMsg = (seq: number, text: string) =>
	entryRow(seq, {
		type: 'message',
		role: 'assistant',
		content: [{ type: 'text', text }],
		usage: { input: 5, output: 2, cacheRead: 0, cacheWrite: 0, reasoning: 0 },
		originRpc: null,
	});

// ── snapshot / entry folding ──────────────────────────────────────────────

describe('snapshot → entries → projections fold', () => {
	it('a fixture snapshot seeds the window, projections and ready flag', () => {
		const { store, item } = openStore();
		store.dispatch(item(snapshotFrame));
		const t = thread(store);
		expect(t?.ready).toBe(true);
		expect(t?.loading).toBe(false);
		expect(t?.cwd).toBe('/proj');
		// The single user message row folded into a `user` item.
		expect(t?.items).toHaveLength(1);
		expect(t?.items[0]?.kind).toBe('user');
		expect((t?.items[0] as { text: string }).text).toBe('hello');
	});

	it('live entry frames fold into items in order', () => {
		const { store, item } = openStore();
		store.dispatch(item(snapshot([userMsg(0, 'hello')])));
		store.dispatch(item({ type: 'entry', seq: 1, event: { type: 'turnStart' } }));
		store.dispatch(item({ type: 'entry', seq: 2, event: { type: 'agentTextDelta', s: 'hi' } }));
		store.dispatch(item({ type: 'entry', seq: 3, event: { type: 'agentTextDelta', s: '! there' } }));
		const t = thread(store);
		const assistant = t?.items.filter((i) => i.kind === 'assistant');
		expect(assistant).toHaveLength(1);
		expect((assistant[0] as { text: string }).text).toBe('hi! there');
	});

	it('the durable assistant row finalizes the streamed draft (no duplicate)', () => {
		const { store, item } = openStore();
		store.dispatch(item(snapshot([userMsg(0, 'hi')])));
		store.dispatch(item({ type: 'entry', seq: 1, event: { type: 'agentTextDelta', s: 'hel' } }));
		store.dispatch(item({ type: 'entry', seq: 2, event: { type: 'message', role: 'assistant', content: [{ type: 'text', text: 'hello world' }], usage: null, originRpc: null } }));
		const t = thread(store);
		const assistant = t?.items.filter((i) => i.kind === 'assistant');
		expect(assistant).toHaveLength(1);
		expect((assistant[0] as { text: string }).text).toBe('hello world');
	});

	it('projection frames update typed fields with higher-seq-wins', () => {
		const { store, item } = openStore();
		store.dispatch(item(snapshotFrame));
		store.dispatch(
			item({
				type: 'projections',
				sessionId: SESSION,
				asOfSeq: 3,
				values: { title: 'v2 migration', running: true },
			}),
		);
		expect(thread(store)?.title).toBe('v2 migration');
		expect(thread(store)?.turnActive).toBe(true);
		// A stale (lower seq) frame must not regress the value.
		store.dispatch(
			item({
				type: 'projections',
				sessionId: SESSION,
				asOfSeq: 2,
				values: { title: 'stale' },
			}),
		);
		expect(thread(store)?.title).toBe('v2 migration');
	});

	it('the model projection maps to a canonical display ref', () => {
		const { store, item } = openStore();
		store.dispatch(item(snapshotFrame));
		store.dispatch(
			item({
				type: 'projections',
				sessionId: SESSION,
				asOfSeq: 4,
				values: { model: { provider: 'DeepSeek-anthropic', modelId: 'deepseek-chat' } },
			}),
		);
		expect(thread(store)?.modelRef).toBe('DeepSeek-anthropic/deepseek-chat');
	});

	it('permission_mode snake_case projection normalizes to the client union', () => {
		const { store, item } = openStore();
		store.dispatch(item(snapshotFrame));
		store.dispatch(
			item({
				type: 'projections',
				sessionId: SESSION,
				asOfSeq: 5,
				values: { permission_mode: 'danger_full_access' },
			}),
		);
		expect(thread(store)?.approvalMode).toBe('danger-full-access');
	});

	it('the running projection falling edge records the turn duration', () => {
		const { store, item } = openStore();
		store.dispatch(item(snapshotFrame));
		store.dispatch(
			item({ type: 'projections', sessionId: SESSION, asOfSeq: 6, values: { running: true } }),
		);
		expect(thread(store)?.turnActive).toBe(true);
		store.dispatch(
			item({ type: 'projections', sessionId: SESSION, asOfSeq: 7, values: { running: false } }),
		);
		expect(thread(store)?.turnActive).toBe(false);
		expect(thread(store)?.lastTurnDurationSec).toBeTypeOf('number');
	});
});

// ── echo retirement ───────────────────────────────────────────────────────

describe('optimistic echo retirement', () => {
	it('an echo is retired when the durable user entry carries its originRpc', () => {
		const { store, item } = openStore();
		// No snapshot yet: echo stands alone.
		store.echoUser(SESSION, 'ping', undefined, { originRpc: 'rpc-42' });
		expect(items(store)).toHaveLength(1);
		expect((items(store)[0] as { id: string }).id).toMatch(/^echo-/);
		// Durable row with matching origin arrives on the snapshot.
		store.dispatch(item(snapshot([userMsg(0, 'ping', 'rpc-42')])));
		// The durable entry replaced the echo (exactly one user item).
		const userItems = items(store).filter((i) => i.kind === 'user');
		expect(userItems).toHaveLength(1);
		expect((userItems[0] as { id: string }).id).toBe('e-0');
	});

	it('an echo whose origin never arrives is not retired', () => {
		const { store, item } = openStore();
		store.echoUser(SESSION, 'hi', undefined, { originRpc: 'rpc-99' });
		store.dispatch(item(snapshot([userMsg(0, 'hi', 'other-rpc')])));
		// Both survive: the durable entry + the unretired echo.
		expect(items(store).filter((i) => i.kind === 'user')).toHaveLength(2);
	});

	it('a live entry frame retires the echo too', () => {
		const { store, item } = openStore();
		store.dispatch(item(snapshot([userMsg(0, 'first')])));
		store.echoUser(SESSION, 'later', undefined, { originRpc: 'rpc-2' });
		expect(items(store).filter((i) => i.kind === 'user')).toHaveLength(2);
		store.dispatch(item({ type: 'entry', seq: 1, event: { type: 'message', role: 'user', content: [{ type: 'text', text: 'later' }], usage: null, originRpc: 'rpc-2' } }));
		const userItems = items(store).filter((i) => i.kind === 'user');
		expect(userItems).toHaveLength(2); // durable first + durable later
		expect((userItems[0] as { id: string }).id).toBe('e-0');
		expect((userItems[1] as { id: string }).id).toBe('e-1');
	});
});

// ── resync / reconnect ────────────────────────────────────────────────────

describe('resync and reseat', () => {
	it('StreamEnd{resync} re-issues a follow stream (seamless resume)', () => {
		const { store, effects, item, end } = openStore();
		store.dispatch(item(snapshot([userMsg(0, 'hello')])));
		const first = effects.openStream.mock.calls[0]![1] as string;
		effects.openStream.mockClear();
		store.dispatch(end({ type: 'resync' }));
		// A new follow request is issued on the same stream id (reseat).
		expect(effects.openStream).toHaveBeenCalledWith(SESSION, first);
		// The next snapshot resumes at the same tail — seamless.
		store.dispatch(item(snapshot([userMsg(0, 'hello')])));
		expect(thread(store)?.ready).toBe(true);
	});

	it('a snapshot behind the last applied tail is a violation → re-follow', () => {
		const { store, effects, item, end } = openStore();
		store.dispatch(item(snapshot([userMsg(0, 'a'), userMsg(1, 'b')]))); // tail 1
		effects.openStream.mockClear();
		store.dispatch(end({ type: 'resync' })); // restart
		const before = effects.openStream.mock.calls.length;
		store.dispatch(item(snapshot([userMsg(0, 'a')]))); // resume at tail 0 < 1
		// The engine rejected the behind resume; the store re-followed.
		expect(effects.openStream.mock.calls.length).toBeGreaterThan(before);
	});

	it('an entry beyond the window opens a gap and re-follows (L5)', () => {
		const { store, effects, item } = openStore();
		store.dispatch(item(snapshot([userMsg(0, 'hello')]))); // tail 0
		const before = effects.openStream.mock.calls.length;
		store.dispatch(item({ type: 'entry', seq: 5, event: { type: 'turnStart' } }));
		// Gap repair reads nothing (async PageHistory is not the engine's
		// sync source) → violation → resync.
		expect(effects.openStream.mock.calls.length).toBeGreaterThan(before);
	});

	it('reseat rotates a fresh stream id and re-follows each live session', () => {
		const { store, effects, item } = openStore();
		store.dispatch(item(snapshot([userMsg(0, 'hello')])));
		const first = effects.openStream.mock.calls[0]![1] as string;
		effects.openStream.mockClear();
		store.reseat();
		// A new connection generation gets a NEW stream id (so a stale
		// Closed/snapshot from the old generation cannot collide, §D.1).
		expect(effects.openStream).toHaveBeenCalledWith(SESSION, expect.any(String));
		const next = effects.openStream.mock.calls[0]![1] as string;
		expect(next).not.toBe(first);
		// The old window stays published until the new snapshot lands.
		expect(thread(store)?.items.some((i) => i.kind === 'user')).toBe(true);
		// The resume snapshot on the new id lands seamlessly (tail 0 >= 0).
		store.dispatch({ kind: 'streamItem', streamId: next, frame: snapshot([userMsg(0, 'hello')]) } as unknown as FromServer);
		expect(thread(store)?.ready).toBe(true);
	});
});

// ── SessionStatus monotonic mirror ────────────────────────────────────────

describe('Host SessionStatus mirror', () => {
	const rowOf = (running = false, errored = false): FromServer =>
		note({
			method: 'threadsUpdated',
			threads: [
				{
					id: SESSION,
					title: 't',
					updated_at: 1,
					running,
					unread: false,
					errored,
					pending_auth: false,
					pending_plan: false,
					background_work: false,
					model_id: 'm',
					pinned: false,
					archived: false,
					parent_id: null,
					depth: 0,
				},
			],
		});

	const status = (partial: Record<string, unknown>): FromServer =>
		host({
			type: 'sessionStatus',
			sessionId: SESSION,
			running: null,
			errored: null,
			unread: null,
			pendingAuth: null,
			pendingPlan: null,
			backgroundWork: null,
			...partial,
		});

	const row = (store: Store) => store.get().threads.find((r) => r.id === SESSION);

	it('mirrors running / pendingAuth / backgroundWork latest-wins', () => {
		const { store } = openStore();
		store.dispatch(rowOf());
		store.dispatch(status({ running: true, pendingAuth: true, backgroundWork: true }));
		expect(row(store)?.running).toBe(true);
		expect(row(store)?.pending_auth).toBe(true);
		expect(row(store)?.background_work).toBe(true);
		store.dispatch(status({ running: false }));
		expect(row(store)?.running).toBe(false);
	});

	it('errored is a set edge: a later false does not clear it', () => {
		const { store } = openStore();
		store.dispatch(rowOf(true));
		store.dispatch(status({ errored: true }));
		expect(row(store)?.errored).toBe(true);
		store.dispatch(status({ errored: false }));
		// Monotonic: errored stays until the next ThreadsUpdated snapshot.
		expect(row(store)?.errored).toBe(true);
	});

	it('unread only increases until focus (the active row never sticks)', () => {
		const { store } = openStore();
		store.dispatch(rowOf());
		// SESSION is the active thread (openRemote switched to it).
		store.dispatch(status({ unread: true }));
		expect(row(store)?.unread).toBe(false);
	});

	it('unread sticks for non-focused rows and clears on a false edge', () => {
		const store = new Store();
		const effects = spyEffects();
		store.attachEffects(effects);
		store.dispatch(
			note({
				method: 'threadsUpdated',
				threads: [
					{
						id: 'other',
						title: 'o',
						updated_at: 1,
						running: false,
						unread: false,
						errored: false,
						pending_auth: false,
						pending_plan: false,
						background_work: false,
						model_id: 'm',
						pinned: false,
						archived: false,
						parent_id: null,
						depth: 0,
					},
				],
			}),
		);
		store.dispatch(
			host({
				type: 'sessionStatus',
				sessionId: 'other',
				running: null,
				errored: null,
				unread: true,
				pendingAuth: null,
				pendingPlan: null,
				backgroundWork: null,
			}),
		);
		expect(store.get().threads[0]?.unread).toBe(true);
		// Focus clears it (client-owned selection).
		store.openLocal('other');
		expect(store.get().threads[0]?.unread).toBe(false);
	});
});

// ── guard tolerance ───────────────────────────────────────────────────────

describe('guard tolerance', () => {
	it('unknown host event tags are dropped, not applied', () => {
		const { store } = openStore();
		const before = store.get().threads.length;
		store.dispatch(host({ type: 'frobnicated', whatever: 1 }));
		expect(store.get().threads).toHaveLength(before);
	});

	it('unknown stream frame types are dropped', () => {
		const { store, item } = openStore();
		store.dispatch(item(snapshotFrame));
		const before = items(store).length;
		store.dispatch(item({ type: 'nonsense', foo: 1 }));
		expect(items(store)).toHaveLength(before);
	});

	it('unknown journal tags on live entries are dropped (L12)', () => {
		const { store, item } = openStore();
		store.dispatch(item(snapshot([userMsg(0, 'hello')])));
		const before = items(store).length;
		store.dispatch(item({ type: 'entry', seq: 1, event: { type: 'notAJournalTag' } }));
		expect(items(store)).toHaveLength(before);
	});

	it('the full §C.2 vocabulary parses through guards without throwing', () => {
		const { store, item } = openStore();
		store.dispatch(item(snapshot(journalEntries as unknown[])));
		expect(thread(store)?.ready).toBe(true);
		expect(thread(store)!.items.length).toBeGreaterThan(0);
	});
});

// ── serverCall adjudication cards ─────────────────────────────────────────

describe('ServerCall cards', () => {
	it('an approve call adds an approval card; decide removes it', () => {
		const { store, item } = openStore();
		store.dispatch(
			callReq('a1', {
				method: 'approve',
				sessionId: SESSION,
				authId: 'a1',
				toolName: 'Bash',
				summary: 'run ls',
				input: {},
			}),
		);
		expect(items(store).some((i) => i.kind === 'approval' && i.id === 'a1')).toBe(true);
		// A rebuilt journal window must never drop a pending card.
		store.dispatch(item(snapshot([userMsg(0, 'x')])));
		expect(items(store).some((i) => i.kind === 'approval' && i.id === 'a1')).toBe(true);
		store.decideApproval(SESSION, 'a1');
		expect(items(store).some((i) => i.kind === 'approval')).toBe(false);
	});

	it('a planVerdict call upserts a single review card', () => {
		const { store } = openStore();
		store.dispatch(
			callReq('s1', {
				method: 'planVerdict',
				sessionId: SESSION,
				planFile: '/p.md',
				title: 'Ship it',
				content: '# plan',
			}),
		);
		expect(items(store).filter((i) => i.kind === 'plan_review')).toHaveLength(1);
	});
});

// ── v1 control/registry notes (still emitted at wave/2) ───────────────────

describe('v1 control/registry notes and host mirrors', () => {
	it('models flow into global state from both the note and the host tag', () => {
		const { store } = openStore();
		const models = (hostEvents.find((e) => e.type === 'models') as { models: unknown[] }).models;
		store.dispatch(note({ method: 'models', models }));
		expect(store.get().models).toHaveLength(1);
		store.dispatch(host({ type: 'models', models: [] }));
		expect(store.get().models).toHaveLength(0);
	});

	it('a sessionCreated switches to conversation view', () => {
		const store = new Store();
		const effects = spyEffects();
		store.attachEffects(effects);
		store.dispatch(note({ method: 'sessionCreated', sessionId: 'fresh' }));
		expect(store.get().view).toBe('conversation');
		expect(store.get().activeThreadId).toBe('fresh');
	});
});

// ── draft lifecycle ───────────────────────────────────────────────────────

describe('draft thread lifecycle', () => {
	it('draft → confirmDraft rekeys state and the echo rides the originRpc', () => {
		const store = new Store();
		const effects = spyEffects();
		store.attachEffects(effects);
		store.draftThread('local-1', 'hello world', undefined, { originRpc: 'rpc-7' });
		expect(store.isCreating('local-1')).toBe(true);
		expect(store.get().activeThreadId).toBe('local-1');
		store.confirmDraft('local-1', 'server-9');
		expect(store.get().activeThreadId).toBe('server-9');
		expect(store.get().perThread['server-9']?.items.some((i) => i.kind === 'user')).toBe(true);
		// The rekeyed session is live: the snapshot retires the echo.
		store.openRemote('server-9');
		const streamId = effects.openStream.mock.calls.at(-1)![1] as string;
		store.dispatch(
			{
				kind: 'streamItem',
				streamId,
				frame: snapshot([userMsg(0, 'hello world', 'rpc-7')]),
			} as unknown as FromServer,
		);
		expect(
			store.get().perThread['server-9']?.items.filter((i) => i.kind === 'user'),
		).toHaveLength(1);
	});
});

// ── §E.3 conversation-info edge signal + write seam ──────────────────────
// The store no longer pulls the Q face (that moved to the conversation-info
// plugin — see `plugins/conversation-info/client.test.ts`). It owns only the
// durable `committed` edge signal (advanced exactly when the fold can change)
// and the `setConversationInfo` write seam the plugin parks its payload on.

describe('conversation-info edge signal (committed) + write seam', () => {
	const fakeInfo: ConversationInfo = {
		threadId: SESSION,
		cursor: 1,
		title: null,
		cwd: null,
		project: null,
		model: null,
		contextWindow: null,
		turns: 0,
		messages: 0,
		models: [],
		cumulativeCost: 0,
		git: null,
	};

	it('committed advances only on durable message rows (the §E.3 refresh edge signal)', () => {
		const pin = new Store();
		const pinOpen = vi.fn();
		pin.attachEffects({ openStream: pinOpen, pageHistory: vi.fn(async () => null) });
		pin.openRemote(SESSION);
		const id = pinOpen.mock.calls[0]![1] as string;
		const pinItem = (frame: unknown): FromServer =>
			({ kind: 'streamItem', streamId: id, frame }) as unknown as FromServer;
		expect(pin.get().perThread[SESSION]?.committed).toBe(0);
		// Snapshot with one committed user message → committed = 1.
		pin.dispatch(pinItem(snapshot([userMsg(0, 'a')])));
		expect(pin.get().perThread[SESSION]?.committed).toBe(1);
		// Non-message rows (deltas) do not advance the edge signal.
		pin.dispatch(pinItem({ type: 'entry', seq: 1, event: { type: 'agentTextDelta', s: 'hi' } }));
		expect(pin.get().perThread[SESSION]?.committed).toBe(1);
		// An assistant message row is a commit.
		pin.dispatch(pinItem({ type: 'entry', seq: 2, event: { type: 'message', role: 'assistant', content: [{ type: 'text', text: 'yo' }], usage: null, originRpc: null } }));
		expect(pin.get().perThread[SESSION]?.committed).toBe(2);
	});

	it('setConversationInfo parks the plugin payload on the thread state', () => {
		const pin = new Store();
		const pinOpen = vi.fn();
		pin.attachEffects({ openStream: pinOpen, pageHistory: vi.fn(async () => null) });
		pin.openRemote(SESSION);
		expect(pin.get().perThread[SESSION]?.conversationInfo).toBeNull();
		pin.setConversationInfo(SESSION, fakeInfo);
		expect(pin.get().perThread[SESSION]?.conversationInfo).toBe(fakeInfo);
		// A repeat write with the same reference is a no-op (no new patch).
		const before = pin.get();
		pin.setConversationInfo(SESSION, fakeInfo);
		expect(pin.get()).toBe(before);
	});
});

// ── §D.2 PageHistory paging (prepend) ────────────────────────────────────

describe('history paging', () => {
	it('requestOlder prepends a backwards page into the window', async () => {
		const pin = new Store();
		const pinOpen = vi.fn();
		pin.attachEffects({
			openStream: pinOpen,
			pageHistory: vi.fn(async () => ({
				// The page must adjoin the window head (seq 5) ⇒ ends at 4.
				records: [entryRow(4, { type: 'message', role: 'user', content: [{ type: 'text', text: 'old' }], usage: null, originRpc: null })] as unknown as WireRecord[],
				hasMore: false,
			})),
		});
		pin.openRemote(SESSION);
		const id = pinOpen.mock.calls[0]![1] as string;
		const pinItem = (frame: unknown): FromServer =>
			({ kind: 'streamItem', streamId: id, frame }) as unknown as FromServer;
		pin.dispatch(pinItem(snapshot([userMsg(5, 'new')])));
		expect(pin.hasMoreHistory(SESSION)).toBe(true);
		await pin.requestOlder(SESSION);
		const users = pin.get().perThread[SESSION]!.items.filter((i) => i.kind === 'user');
		expect(users.map((u) => (u as { text: string }).text)).toEqual(['old', 'new']);
		expect(pinOpen).toHaveBeenCalledTimes(1);
	});
});
