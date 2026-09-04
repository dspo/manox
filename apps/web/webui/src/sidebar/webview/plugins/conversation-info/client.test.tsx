// @vitest-environment jsdom
// The conversation-info acceptance sample (§H). The plugin's contract:
//   - it contributes a header-utilities chip through the slot tree (never
//     importing the header), toggling the info card;
//   - the Q-face GetConversationInfo pull is edge-triggered on the durable
//     `committed` count (not on streaming deltas, not on a wall clock),
//     debounced 120ms and visibility-aware, and reaches the store ONLY through
//     the public `setConversationInfo` write seam;
//   - zero notes, zero emit points, no store-internal writes.
//
// The store is mocked to a controllable face (subscribe/get/setConversationInfo
// + a `committed` edge we can advance) and the api fetch seam is mocked, so the
// suite drives exactly the plugin's inputs.

import { act, createElement } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ChatState, ThreadState } from '../../state/store';

// ── controllable seams the plugin imports ──────────────────────────────────
const listeners = new Set<() => void>();
let committed = 0;
let parkedInfo: ConversationPayload | null = null;
const setConversationInfo = vi.fn((_sessionId: string, info: ConversationPayload | null) => {
	parkedInfo = info;
});
type ConversationPayload = { threadId: string; cursor: number; models: unknown[]; cumulativeCost: number };

function threadState(): ThreadState {
	return {
		sessionId: 's1',
		conversationInfo: parkedInfo,
		committed,
		items: [],
		subagents: [],
		subagentChildren: {},
		plan: null,
		goal: null,
	} as unknown as ThreadState;
}
let state: ChatState;
function publish(): void {
	state = {
		threads: [{ id: 's1' }],
		perThread: { s1: threadState() },
		activeThreadId: 's1',
		models: [],
		commands: [],
		view: 'conversation',
		error: null,
	} as unknown as ChatState;
	for (const l of [...listeners]) l();
}
/** Advance the durable committed edge (the §E.3 refresh signal) and notify. */
function advanceCommitted(n = 1): void {
	committed += n;
	publish();
}

vi.mock('../../state/bridge', () => ({
	store: {
		subscribe: (l: () => void) => {
			listeners.add(l);
			return () => {
				listeners.delete(l);
			};
		},
		get: () => state,
		projection: () => undefined,
		setConversationInfo,
	},
}));

// The Q-face fetch seam: every call resolves a fold payload we can inspect.
const getConversationInfo = vi.fn(async (sessionId: string) => ({
	threadId: sessionId,
	cursor: 1,
	models: [],
	cumulativeCost: 0,
}));
vi.mock('../../api/client', () => ({
	getConversationInfo: (sessionId: string) => getConversationInfo(sessionId),
	// The info-card body references ThreadApi for goal actions; stub it out.
	ThreadApi: class {
		goal(): void {
			/* no-op in the sample */
		}
	},
}));

// Import the plugin AFTER the mocks: loading its module runs the `inject` +
// `register` side effects against the real slot core (its `slots.registry`
// import declares the header-utilities slot first, so the contribution lands).
const { Slot } = await import('../../slots.outlet');
await import('./client');
const { entriesOfSlot } = await import('../../state/slots');

// ── render harness ─────────────────────────────────────────────────────────
let root: Root | null = null;
function renderHeader(): void {
	const host = document.createElement('div');
	document.body.appendChild(host);
	root = createRoot(host);
	act(() => {
		root!.render(
			createElement(Slot, {
				name: 'conversation.session.header.utilities',
				owner: { sessionId: 's1', models: [] },
			}),
		);
	});
}
function teardown(): void {
	if (root) act(() => root!.unmount());
	root = null;
	document.body.innerHTML = '';
}

beforeEach(() => {
	vi.useFakeTimers();
	committed = 0;
	parkedInfo = null;
	setConversationInfo.mockClear();
	getConversationInfo.mockClear();
	publish();
	(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
});
afterEach(() => {
	teardown();
	vi.useRealTimers();
	vi.unstubAllGlobals();
});

describe('conversation-info plugin contribution', () => {
	it('injects a header-utilities chip into the slot (no header import)', () => {
		const chip = entriesOfSlot('conversation.session.header.utilities').find(
			(e) => e.id === 'conversation-info',
		);
		expect(chip).toBeDefined();
		expect(chip!.registrant).toBe('conversation-info');
	});

	it('edge-triggers a debounced pull on a committed advance (not on a delta)', async () => {
		renderHeader();
		// The plugin warms the payload once on mount, after the debounce.
		await act(async () => {
			await vi.advanceTimersByTimeAsync(120);
		});
		expect(getConversationInfo).toHaveBeenCalledTimes(1);
		expect(getConversationInfo).toHaveBeenCalledWith('s1');

		// A committed edge schedules another pull, but only after the debounce.
		act(() => advanceCommitted());
		await act(async () => {
			await vi.advanceTimersByTimeAsync(60);
		});
		expect(getConversationInfo).toHaveBeenCalledTimes(1); // still coalescing
		await act(async () => {
			await vi.advanceTimersByTimeAsync(60);
		});
		expect(getConversationInfo).toHaveBeenCalledTimes(2);
		// The result reached the store through the public write seam only.
		expect(setConversationInfo).toHaveBeenCalledWith('s1', {
			threadId: 's1',
			cursor: 1,
			models: [],
			cumulativeCost: 0,
		});
	});

	it('defers the pull while the tab is hidden and flushes on becoming visible', async () => {
		const hidden = { value: true };
		Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden.value });
		renderHeader();
		await act(async () => {
			await vi.advanceTimersByTimeAsync(120);
		});
		// Hidden at the debounce edge → the fetch was skipped, not dropped.
		expect(getConversationInfo).not.toHaveBeenCalled();
		// Becoming visible flushes it immediately.
		hidden.value = false;
		act(() => {
			document.dispatchEvent(new Event('visibilitychange'));
		});
		expect(getConversationInfo).toHaveBeenCalledTimes(1);
	});
});
