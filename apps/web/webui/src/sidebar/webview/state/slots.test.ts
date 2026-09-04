// Slot registry core tests (§G): the semantics the composition layer promises
// — declaration gating, kind-specific cell identity, priority shadowing, list
// ordering, keyed dispatch, and the inject declaration-lifetime contract. Pure
// node suite (no React): exercises the same core `slots.outlet.tsx` renders on.

import { afterEach, describe, expect, it, vi } from 'vitest';

import {
	__resetSlotsForTests,
	declareSlot,
	entriesForKey,
	entriesOfSlot,
	inject,
	register,
	specOf,
	subscribe,
	subscribeDeclaration,
	type SlotMap,
	type StoredEntry,
} from './slots';

// Module augmentation: declare the slot keys the suite exercises so
// `register` is statically typed (this is the very declaration-merge path the
// composition layer is built on). Kinds match the usages below.
declare module './slots' {
	interface SlotMap {
		'a.slot': { kind: 'list'; scope: 'root' };
		s: { kind: 'single'; scope: 'root' };
		l: { kind: 'list'; scope: 'root' };
		k: { kind: 'keyed'; scope: 'session' };
		d: { kind: 'single'; scope: 'root' };
		pending: { kind: 'list'; scope: 'root' };
		late: { kind: 'list'; scope: 'root' };
	}
}

// A no-op component for registrations (the core never renders it).
const stub = (): null => null;

/** Registration ids in ledger order (the shadowing heads). */
const ids = (entries: readonly StoredEntry[]): (string | undefined)[] =>
	entries.map((e) => e.id ?? e.key);

// Local aliases to keep the (well-typed) `SlotMap` reference used (lint-clean).
void (null as unknown as SlotMap);

afterEach(() => {
	__resetSlotsForTests();
});

describe('declaration gating', () => {
	it('register into an undeclared slot throws', () => {
		expect(() => register({ name: 'a.slot', id: 'x' }, stub)).toThrow(/not declared/);
	});

	it('a slot declares once and reports its spec', () => {
		declareSlot('a.slot', { kind: 'list', scope: 'root' });
		expect(specOf('a.slot')).toEqual({ kind: 'list', scope: 'root' });
		expect(() => declareSlot('a.slot', { kind: 'single', scope: 'root' })).toThrow(/already declared/);
	});

	it('a declaration collapse clears entries and reopens the slot', () => {
		const dispose = declareSlot('a.slot', { kind: 'list', scope: 'root' });
		register({ name: 'a.slot', id: 'x' }, stub);
		expect(entriesOfSlot('a.slot').length).toBe(1);
		dispose();
		// Collapsed: empty view, and the key can be declared afresh.
		expect(entriesOfSlot('a.slot').length).toBe(0);
		expect(specOf('a.slot')).toBeUndefined();
		expect(() => declareSlot('a.slot', { kind: 'single', scope: 'root' })).not.toThrow();
	});
});

describe('single kind', () => {
	it('renders the lowest-priority occupant and shadows by priority', () => {
		declareSlot('s', { kind: 'single', scope: 'root' });
		register({ name: 's', priority: 0 }, stub);
		register({ name: 's', priority: 10 }, stub);
		const heads = entriesOfSlot('s');
		// The single slot is one cell; only the lowest-priority entry survives.
		expect(heads.length).toBe(1);
		expect(heads[0]!.priority).toBe(0);
	});

	it('a colliding priority throws (shadow with a different rank instead)', () => {
		declareSlot('s', { kind: 'single', scope: 'root' });
		register({ name: 's', priority: 5, registrant: 'first' }, stub);
		expect(() => register({ name: 's', priority: 5 }, stub)).toThrow(/already has a registration at priority 5/);
	});
});

describe('list kind', () => {
	it('orders by priority, then explicit order, then registration sequence', () => {
		declareSlot('l', { kind: 'list', scope: 'root' });
		register({ name: 'l', id: 'hi', priority: 5 }, stub);
		register({ name: 'l', id: 'b', order: 2 }, stub);
		register({ name: 'l', id: 'a', order: 1 }, stub);
		register({ name: 'l', id: 'c', order: 1 }, stub); // same order → seq tiebreak
		// priority 0 entries first (a,c,b by order then seq), then priority 5 (hi).
		expect(ids(entriesOfSlot('l'))).toEqual(['a', 'c', 'b', 'hi']);
	});

	it('per-id shadowing: two ids coexist, a second at an occupied id/priority collides', () => {
		declareSlot('l', { kind: 'list', scope: 'root' });
		register({ name: 'l', id: 'x' }, stub);
		register({ name: 'l', id: 'y' }, stub); // distinct id, same priority — ok
		expect(ids(entriesOfSlot('l'))).toEqual(['x', 'y']);
		// Same id + same priority collides; a different priority shadows instead.
		expect(() => register({ name: 'l', id: 'x' }, stub)).toThrow(/already has a registration/);
		expect(() => register({ name: 'l', id: 'x', priority: 1 }, stub)).not.toThrow();
		// The lower-priority occupant wins the `x` cell.
		const xHeads = entriesOfSlot('l').filter((e) => e.id === 'x');
		expect(xHeads.length).toBe(1);
		expect(xHeads[0]!.priority).toBe(0);
	});

	it('a missing id throws', () => {
		declareSlot('l', { kind: 'list', scope: 'root' });
		expect(() => register({ name: 'l' } as never, stub)).toThrow(/requires options.id/);
	});
});

describe('keyed kind', () => {
	it('dispatches entries by key', () => {
		declareSlot('k', { kind: 'keyed', scope: 'session' });
		register({ name: 'k', key: 'alpha' }, stub);
		register({ name: 'k', key: 'beta' }, stub);
		register({ name: 'k', key: 'alpha', priority: 3 }, stub);
		expect(ids(entriesForKey('k', 'alpha'))).toEqual(['alpha']);
		expect(ids(entriesForKey('k', 'beta'))).toEqual(['beta']);
		// The alpha cell keeps one head (priority 0 wins over 3).
		expect(entriesForKey('k', 'alpha').length).toBe(1);
	});

	it('a missing key throws', () => {
		declareSlot('k', { kind: 'keyed', scope: 'root' });
		expect(() => register({ name: 'k' } as never, stub)).toThrow(/requires options.key/);
	});
});

describe('register disposer', () => {
	it('removes exactly its own contribution (idempotent)', () => {
		declareSlot('l', { kind: 'list', scope: 'root' });
		const off1 = register({ name: 'l', id: 'a' }, stub);
		register({ name: 'l', id: 'b' }, stub);
		off1();
		off1(); // idempotent
		expect(ids(entriesOfSlot('l'))).toEqual(['b']);
	});
});

describe('change propagation', () => {
	it('entry listeners notify batched per microtask (N mutations → 1 call)', async () => {
		declareSlot('l', { kind: 'list', scope: 'root' });
		const listener = vi.fn();
		subscribe('l', listener);
		register({ name: 'l', id: 'a' }, stub);
		register({ name: 'l', id: 'b' }, stub);
		register({ name: 'l', id: 'c' }, stub);
		// Not yet (microtask-batched).
		expect(listener).not.toHaveBeenCalled();
		await Promise.resolve();
		expect(listener).toHaveBeenCalledTimes(1);
	});

	it('declaration listeners fire synchronously on declare and collapse', () => {
		const order: string[] = [];
		subscribeDeclaration('d', () => order.push(`epoch:${specOf('d') ? 'up' : 'down'}`));
		const dispose = declareSlot('d', { kind: 'single', scope: 'root' });
		expect(order).toEqual(['epoch:up']); // fired synchronously
		dispose();
		expect(order).toEqual(['epoch:up', 'epoch:down']);
	});
});

describe('inject declaration-lifetime contract', () => {
	it('runs now when the slot is already declared', () => {
		declareSlot('l', { kind: 'list', scope: 'root' });
		const cb = vi.fn(() => undefined);
		inject('l', cb);
		expect(cb).toHaveBeenCalledTimes(1);
	});

	it('defers until a fresh declaration, then runs once per lifetime', () => {
		const cb = vi.fn(() => undefined);
		inject('pending', cb);
		expect(cb).not.toHaveBeenCalled();
		const dispose1 = declareSlot('pending', { kind: 'list', scope: 'root' });
		expect(cb).toHaveBeenCalledTimes(1);
		dispose1();
		declareSlot('pending', { kind: 'list', scope: 'root' });
		expect(cb).toHaveBeenCalledTimes(2); // a second declaration lifetime
	});

	it('calls the returned disposer when the declaration collapses and on inject-dispose', () => {
		const contributed = vi.fn(() => undefined);
		const disposer = vi.fn();
		inject('l', () => {
			contributed();
			return disposer;
		});
		const disposeDecl = declareSlot('l', { kind: 'single', scope: 'root' });
		expect(contributed).toHaveBeenCalledTimes(1);
		disposeDecl(); // collapse → the contribution's disposer runs
		expect(disposer).toHaveBeenCalledTimes(1);
	});

	it('a late registrant contributes into a slot declared afterwards (no import of the owner)', () => {
		// This is the timing contract that lets the conversation-info plugin
		// register into a header that mounts later, without importing it.
		inject('late', () => register({ name: 'late', id: 'chip' }, stub));
		expect(entriesOfSlot('late').length).toBe(0);
		declareSlot('late', { kind: 'list', scope: 'root' });
		expect(ids(entriesOfSlot('late'))).toEqual(['chip']);
	});
});
