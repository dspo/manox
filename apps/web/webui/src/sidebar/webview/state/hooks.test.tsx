// @vitest-environment jsdom
// Standard hooks tests (§G): the referential-stability contract slot
// components rely on. The store is mocked to a controllable object so the
// suite tests the hook vocabulary itself, not the fold.

import { createElement, type ReactNode } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ChatState } from './store';

// Mock the bridge store: `get` returns a mutable `state`, `subscribe` fans out
// to listeners, `projection` returns a frozen per-key slot. This is the only
// surface the hooks touch.
const listeners = new Set<() => void>();
let state: ChatState;
// Store the slot already frozen so `projection()` hands out a stable
// reference between changes — mirroring the real store's §E.2 contract (the
// `useProjection` uSES source must not build a fresh object per read).
const projections = new Map<string, Readonly<{ value: unknown; seq: number }>>();
function setProjection(sessionId: string, key: string, value: unknown, seq: number): void {
	projections.set(`${sessionId}:${key}`, Object.freeze({ value, seq }));
}
vi.mock('./bridge', () => ({
	store: {
		subscribe: (l: () => void) => {
			listeners.add(l);
			return () => {
				listeners.delete(l);
			};
		},
		get: () => state,
		projection: (sessionId: string, key: string) =>
			projections.get(`${sessionId}:${key}`),
		setConversationInfo: vi.fn(),
	},
}));

// vi.mock is hoisted above imports, so these resolve against the mocked bridge.
const { shallow, useProjection, useStore, useThreadStatus } = await import('./hooks');

function publish(next: ChatState): void {
	state = next;
	for (const l of [...listeners]) l();
}
function bump(): void {
	for (const l of [...listeners]) l();
}

type Row = { id: string; running?: boolean; unread?: boolean; errored?: boolean };
const emptyState = (threads: Row[], activeThreadId: string | null = null): ChatState =>
	({
		threads,
		perThread: {},
		activeThreadId,
		models: [],
		commands: [],
		view: 'list',
		error: null,
	} as unknown as ChatState);

const flush = (): Promise<void> => act(async () => {});
void flush;

let root: Root | null = null;
/** Mount a probe component, counting committed renders; returns a handle. */
function mount(node: () => ReactNode) {
	const host = document.createElement('div');
	document.body.appendChild(host);
	root = createRoot(host);
	let commits = 0;
	const Probe = (): ReactNode => {
		commits += 1;
		return node();
	};
	act(() => root!.render(createElement(Probe)));
	return {
		get commits() {
			return commits;
		},
		unmount: () => act(() => root!.unmount()),
	};
}

beforeEach(() => {
	listeners.clear();
	projections.clear();
	// React act() needs this flag in 19.
	(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
});
afterEach(() => {
	if (root) act(() => root!.unmount());
	root = null;
	document.body.innerHTML = '';
});

describe('useStore selector stability', () => {
	it('an inline object selector does not re-render when its shallow result is unchanged', () => {
		publish(emptyState([{ id: 'a', running: false }]));
		let captured: { running: boolean } | undefined;
		const view = mount(() => {
			const slice = useStore((s) => ({ running: s.threads[0]?.running === true }), shallow);
			captured = slice;
			return null;
		});
		const commitsAfterMount = view.commits;
		const first = captured;

		// A notification that does NOT touch the selected field: the
		// shallow-stable selector keeps the same reference and never commits.
		act(() => publish(emptyState([{ id: 'a', running: false }], 'other')));
		expect(view.commits).toBe(commitsAfterMount);
		expect(captured).toBe(first);

		// Flip the selected field → the result changes → a commit happens.
		act(() => publish(emptyState([{ id: 'a', running: true }])));
		expect(view.commits).toBe(commitsAfterMount + 1);
		expect(captured!.running).toBe(true);
	});
});

describe('useProjection', () => {
	it('returns the frozen { value, seq } slot and tracks it reactively', () => {
		publish(emptyState([], 's1'));
		let seen: { value: unknown; seq: number } | undefined;
		const view = mount(() => {
			seen = useProjection('model', 's1');
			return null;
		});
		expect(seen).toBeUndefined();
		act(() => {
			setProjection('s1', 'model', { provider: 'p', modelId: 'm' }, 3);
			bump();
		});
		expect(seen).toMatchObject({ seq: 3 });
		expect(Object.isFrozen(seen)).toBe(true);
	});

	it('the selector overload narrows the raw value', () => {
		publish(emptyState([], 's1'));
		setProjection('s1', 'reasoning_effort', 'max', 1);
		let effort: unknown;
		mount(() => {
			effort = useProjection('reasoning_effort', 's1', (slot) => slot?.value ?? null);
			return null;
		});
		expect(effort).toBe('max');
	});
});

describe('useThreadStatus', () => {
	it('mirrors the row flags into a shallow-stable status object', () => {
		publish(emptyState([{ id: 's1', running: true, unread: false }]));
		let status: ReturnType<typeof useThreadStatus>;
		mount(() => {
			status = useThreadStatus('s1');
			return null;
		});
		expect(status!).toMatchObject({ running: true, errored: false, unread: false });
	});
});

void flush;
