// Module-local UI overlay registry (client-owned view state, §F.2: selection
// / focus / overlays are never mirrored in the fold). Overlay ids are loose
// strings; the shell wires them across components without any panel
// importing another panel: the settings trigger flips 'settings', the shell
// overlay outlet reads it, the header utility chip flips 'turn-navigator'
// and the conversation view (the navigator's data owner) reads it.

import { useSyncExternalStore } from 'react';

const openFlags = new Map<string, boolean>();
const listenerSets = new Map<string, Set<() => void>>();

function notify(id: string): void {
	const set = listenerSets.get(id);
	if (set) for (const fn of [...set]) fn();
}

export function isOverlayOpen(id: string): boolean {
	return openFlags.get(id) === true;
}

export function setOverlayOpen(id: string, open: boolean): void {
	if (isOverlayOpen(id) === open) return;
	if (open) openFlags.set(id, true);
	else openFlags.delete(id);
	notify(id);
}

export function toggleOverlay(id: string): void {
	setOverlayOpen(id, !isOverlayOpen(id));
}

/** React binding for one overlay's open flag (stable boolean snapshot). */
export function useOverlayOpen(id: string): boolean {
	return useSyncExternalStore(
		(onChange) => {
			let set = listenerSets.get(id);
			if (!set) {
				set = new Set();
				listenerSets.set(id, set);
			}
			set.add(onChange);
			return () => {
				set?.delete(onChange);
			};
		},
		() => isOverlayOpen(id),
		() => isOverlayOpen(id),
	);
}
