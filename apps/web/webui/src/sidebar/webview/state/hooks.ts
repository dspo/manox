// Standard hooks (§G): the ONLY store-reading vocabulary slot components
// (and any webui component) use. The store object itself never leaves the
// module surface below except through these selectors — components never
// import each other, and private data reaches a component through its
// registration closure (see `state/slots.ts`).
//
// - `useStore(selector)` — the `useSyncExternalStore` wrapper over the
//   folded `ChatState` (T7 store v2). Selector results are stabilized: a
//   selector that rebuilds a value (object/array) per call only re-renders
//   when the result's shallow shape actually changes (dsh store.ts
//   `createSelectorHook` memo discipline).
// - `useProjection(key)` — §E.1/§E.2 sugar over the per-session
//   `{ value, seq }` projection slot: callable with or without a session id
//   (omitted ⇒ the active thread), with a selector narrowing the raw
//   unknown wire value.
// - `useThreadStatus(sessionId)` — the §D.5 `SessionStatus` host-event
//   mirror of the threads-list row (running/errored/unread/pending_auth/
//   pending_plan/background_work), monotonic per the mirror rules.

import { useCallback, useMemo, useRef, useSyncExternalStore } from 'react';

import type {
	GitStats,
	ModelInfo,
	ReasoningEffort,
	ConversationInfo,
	ThreadListItem,
} from '../../../protocol';
import { store } from './bridge';
import type { ChatState, ThreadState } from './store';

/** The observable folded state surface a selector reads. */
export type StoreSelector<T> = (state: ChatState) => T;

const subscribe = (listener: () => void): (() => void) => store.subscribe(listener);

/** Shallow equality for primitive- or one-level-object/array selector results. */
export function shallow(left: unknown, right: unknown): boolean {
	if (Object.is(left, right)) return true;
	if (typeof left !== 'object' || typeof right !== 'object' || left === null || right === null) {
		return false;
	}
	if (Array.isArray(left) !== Array.isArray(right)) return false;
	const a = left as Record<string, unknown>;
	const b = right as Record<string, unknown>;
	const keysA = Object.keys(a);
	const keysB = Object.keys(b);
	if (keysA.length !== keysB.length) return false;
	for (const key of keysA) {
		if (!Object.is(a[key], b[key])) return false;
	}
	return true;
}

/**
 * Read a slice of the store. When `isEqual` is provided (or the selector
 * produces a non-identity-stable value and `shallow` is passed), repeated
 * store notifications only re-render when the selected result changes under
 * the equality — the referential-stability contract slot components rely
 * on (§G: keep observable source and snapshot identities stable).
 */
export function useStore<T>(selector: StoreSelector<T>, isEqual?: (a: T, b: T) => boolean): T {
	// A stable selector identity keeps getSnapshot stable across renders.
	const selectorRef = useRef(selector);
	selectorRef.current = selector;
	const lastResult = useRef<{ has: boolean; value: T | undefined }>({ has: false, value: undefined });

	const getSnapshot = useCallback((): T => {
		const next = selectorRef.current(store.get());
		const cached = lastResult.current;
		if (cached.has && (isEqual ? isEqual(cached.value as T, next) : Object.is(cached.value, next))) {
			return cached.value as T;
		}
		lastResult.current = { has: true, value: next };
		return next;
	}, [isEqual]);

	return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

/** A §E.2 projection seat: whole value + the seq that produced it (higher-seq-wins). */
export interface ProjectionSnapshot {
	value: unknown;
	seq: number;
}

/** Narrow a raw projection value (unknown wire JSON) at the call site. */
export type ProjectionSelector<T> = (value: unknown) => T;

/**
 * The `{ value, seq }` projection slot for `key` of one session. Omit
 * `sessionId` to read the active thread. Returns `undefined` while the
 * projection is not in the client's window (not yet folded).
 */
export function useProjection(key: string): ProjectionSnapshot | undefined;
export function useProjection<T>(key: string, selector: (slot: ProjectionSnapshot | undefined) => T): T;
export function useProjection<T>(
	key: string,
	sessionId: string | null | undefined,
	selector?: (slot: ProjectionSnapshot | undefined) => T,
): T | ProjectionSnapshot | undefined;
export function useProjection(
	key: string,
	sessionIdOrSelector?: string | null | undefined | ((slot: ProjectionSnapshot | undefined) => unknown),
	maybeSelector?: (slot: ProjectionSnapshot | undefined) => unknown,
): unknown {
	const asSelector = typeof sessionIdOrSelector === 'function' ? sessionIdOrSelector : maybeSelector;
	const sessionId = typeof sessionIdOrSelector === 'function' ? undefined : sessionIdOrSelector;
	// The raw snapshot is read through a stable store method (the store's
	// `projection` accessor returns the frozen slot object — stable identity
	// between changes, the uSES contract).
	const read = useCallback(
		(): ProjectionSnapshot | undefined => {
			const target = sessionId ?? store.get().activeThreadId;
			if (target === null) return undefined;
			return store.projection(target, key);
		},
		[key, sessionId],
	);
	const getSnapshot = useCallback(
		() => (asSelector ? asSelector(read()) : read()),
		[read, asSelector],
	);
	return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

/** The §D.5 `SessionStatus` mirror shape a thread row carries. */
export interface ThreadStatus {
	running: boolean;
	errored: boolean;
	unread: boolean;
	pending_auth: boolean;
	pending_plan: boolean;
	background_work: boolean;
}

/**
 * The SessionStatus mirror of one session (rows carry it; the store's
 * `sessionStatus` fold keeps it monotonic per §D.5). Stable result object
 * via shallow equality.
 */
export function useThreadStatus(sessionId: string | null | undefined): ThreadStatus | undefined {
	return useStore(
		(state) => {
			const row = sessionId === null || sessionId === undefined
				? undefined
				: state.threads.find((r) => r.id === sessionId);
			if (!row) return undefined;
			return {
				running: row.running === true,
				errored: row.errored === true,
				unread: row.unread === true,
				pending_auth: row.pending_auth === true,
				pending_plan: row.pending_plan === true,
				background_work: row.background_work === true,
			} satisfies ThreadStatus;
		},
		shallow,
	);
}

/** Read the whole folded state (the shell-level identity read: the store
 * patches immutably, so the snapshot object is already stable between
 * changes — no equality wrapper needed). */
export function useChatState(): ChatState {
	return useStore((state) => state);
}

/** One thread's folded state (`undefined` until the store knows it). */
export function useThreadState(sessionId: string | null | undefined): ThreadState | undefined {
	return useStore((state) => (sessionId ? state.perThread[sessionId] : undefined));
}

/** The §E.3 Q-face payload as carried on the thread state. */
export function useConversationInfo(sessionId: string | null | undefined): ConversationInfo | null {
	return useStore((state) => (sessionId ? (state.perThread[sessionId]?.conversationInfo ?? null) : null));
}

/** The global model registry (host-pushed `Models`). */
export function useModels(): ModelInfo[] {
	return useStore((state) => state.models);
}

/** The threads-list registry. */
export function useThreads(): ThreadListItem[] {
	return useStore((state) => state.threads);
}

/** The active conversation's session id (client-owned view state). */
export function useActiveThreadId(): string | null {
	return useStore((state) => state.activeThreadId);
}

/** A thread's durable committed-message count (the §E.3 refresh edge signal). */
export function useCommittedCount(sessionId: string | null | undefined): number {
	return useStore((state) =>
		sessionId ? (state.perThread[sessionId]?.committed ?? 0) : 0,
	);
}

/** A thread row from the registry by id (title/cwd/model/pin/archive…). */
export function useThreadRow(sessionId: string | null | undefined): ThreadListItem | undefined {
	return useStore(
		(state) => (sessionId ? state.threads.find((r) => r.id === sessionId) : undefined),
	);
}

/** The reasoning-effort projection of a session. */
export function useReasoningEffort(sessionId: string | null | undefined): ReasoningEffort | null {
	const slot = useProjection('reasoning_effort', sessionId);
	return useMemo(() => (typeof slot?.value === 'string' ? (slot.value as ReasoningEffort) : null), [slot]);
}

/** The git stats §E.3 fold payload (branch row of the info card). */
export type { GitStats };
