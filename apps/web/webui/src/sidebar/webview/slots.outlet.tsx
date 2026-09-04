// The React binding of the slot registry (§G): a `<Slot>` outlet renders the
// shadowing winners of one key, re-rendering on registry changes through
// `useSyncExternalStore`. Owners put an outlet at their render site; every
// other surface contributes through `register`/`inject` and never imports
// another component into the layout.

import { createElement, Fragment, type ReactNode, useSyncExternalStore } from 'react';

import {
	entries,
	entriesOfSlot,
	subscribe,
	type OwnerOf,
	type SlotKey,
	type StoredEntry,
} from './state/slots';

/** Stable list of one slot's registered entries (mutation-bumped; the
 * shadowing projection is a render-time read on top). */
function useSlotEntries(key: string): readonly StoredEntry[] {
	return useSyncExternalStore(
		(onChange) => subscribe(key, onChange),
		() => entries(key),
		() => entries(key),
	);
}

export type SlotProps<K extends SlotKey> = {
	/** Target slot key. */
	name: K;
	/** Owner props handed to every entry's component at this render site. */
	owner: OwnerOf<K>;
	/** Render only the entry with this list id (owner-side single-slot projection). */
	only?: string;
	/** Body rendered when the slot has no live entry. */
	fallback?: ReactNode;
};

/**
 * Render one slot: the shadowing winners (single → the slot is one cell;
 * list → one entry per id; keyed → per `key`) in ledger order (priority asc,
 * then `order`, then registration). Returns a fragment — the surrounding
 * layout (flex row, toolbar…) stays with the owner.
 */
export function Slot<K extends SlotKey>({ name, owner, only, fallback }: SlotProps<K>): ReactNode {
	useSlotEntries(name);
	// The frozen entry list is the reactive source; the winner projection is
	// derived at render (fresh array per read by contract, never a snapshot).
	const heads = entriesOfSlot(name).filter(
		(entry) => only === undefined || (entry.id ?? entry.key) === only,
	);
	if (heads.length === 0) return fallback ?? null;
	return (
		<>
			{heads.map((entry) => (
				<Fragment key={entry.id ?? entry.key ?? name}>
					{createElement(entry.component as (props: OwnerOf<K>) => ReactNode, owner)}
				</Fragment>
			))}
		</>
	);
}

/**
 * Render one keyed cell (the dispatch site supplies the key). Same contract
 * as {@link Slot} narrowed to entries whose `key` matches.
 */
export function SlotKeyed<K extends SlotKey>(
	props: SlotProps<K> & { entryKey: string },
): ReactNode {
	const { entryKey, ...rest } = props;
	return (
		<Slot
			{...rest}
			only={entryKey}
		/>
	);
}
