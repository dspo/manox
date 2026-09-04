// First-batch slot declarations (§G). The type contracts merge into
// `state/slots.ts`'s `SlotMap` here (module augmentation — the single
// convergence point for the webui's shipped slot surface), and the runtime
// spec is declared at module load so every `register`/`inject` call has a
// live target regardless of mount order.
//
// The tree (§G 首批落位):
//   root
//   ├─ sidebar.workspaces.footer.action  (list)  — session-list footer actions
//   ├─ conversation.session.header.utilities (list) — header utility chips
//   ├─ conversation.composer.dock        (list)  — composer bottom-row chips
//   ├─ settings.section                  (list)  — settings overlay sections
//   └─ shell.overlay                     (list)  — app-level modal overlays

import type { ModelInfo } from '../../../protocol';
import type { AssertSlotDefs } from './slots';
import { declareSlot } from './slots';

declare module './slots' {
	interface SlotMap {
		/** Actions beside the "New session" button in the workspaces footer. */
		'sidebar.workspaces.footer.action': {
			kind: 'list';
			scope: 'root';
		};
		/** Utility chips in the conversation header (info card, navigator, …).
		 * Strict-session scope: the owner renders it only for an open thread. */
		'conversation.session.header.utilities': {
			kind: 'list';
			scope: 'session';
			owner: { sessionId: string; models: ModelInfo[] };
		};
		/** Chip cluster docked in the composer's bottom row (model picker …). */
		'conversation.composer.dock': {
			kind: 'list';
			scope: 'session-maybe';
			owner: {
				/** Null for the draft (home) composer. */
				sessionId: string | null;
				/** Canonical display ref the chip shows (L8). */
				currentModelRef: string | null;
				disabled: boolean;
				/** Draft-mode selection: the choice rides along on creation. */
				onModelChange?: ((modelId: string) => void) | undefined;
			};
		};
		/** Sections of the settings overlay (General, …). */
		'settings.section': {
			kind: 'list';
			scope: 'root';
		};
		/** App-level modal overlays (the settings sheet first; plugins add
		 * their own via `slots.register`). */
		'shell.overlay': {
			kind: 'list';
			scope: 'root';
			owner: { sessionId: string | null };
		};
	}
}

/** Compile-time guard: every merged entry satisfies the slot contract. */
export type _AssertSlots = AssertSlotDefs;

// Runtime declarations — idempotent across HMR re-evaluation.
declareSlot('sidebar.workspaces.footer.action', {
	kind: 'list',
	scope: 'root',
	declaredBy: 'slots.registry',
});
declareSlot('conversation.session.header.utilities', {
	kind: 'list',
	scope: 'session',
	declaredBy: 'slots.registry',
});
declareSlot('conversation.composer.dock', {
	kind: 'list',
	scope: 'session-maybe',
	declaredBy: 'slots.registry',
});
declareSlot('settings.section', {
	kind: 'list',
	scope: 'root',
	declaredBy: 'slots.registry',
});
declareSlot('shell.overlay', {
	kind: 'list',
	scope: 'root',
	declaredBy: 'slots.registry',
});
