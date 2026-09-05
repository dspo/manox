// conversation-info — the §G/§H acceptance-sample extension plugin.
//
// It reproduces the reference plugin's shape verbatim in the webui's own
// composition vocabulary:
//   - a `conversation.session.header.utilities` chip (the info-card entry)
//     contributed through `inject` + `register` — the plugin never imports
//     the header it appears in, and no other surface imports the plugin;
//   - reads shared state ONLY through the standard store hooks (§E):
//     `useThreadState` / `useConversationInfo` (selectors) and
//     `useCommittedCount` (the §E.3 durable committed-message edge signal);
//   - on a `committed` edge it runs the debounced, visibility-aware
//     `GetConversationInfo` pull through the api client's public Q-face
//     fetch seam and parks the payload back on the store via the public
//     `setConversationInfo` write seam.
//
// Discipline (the §L invariants the sample must prove):
//   - ZERO notes: no request-note / correlation-tag is threaded through the
//     store to carry this fold's result; the durable journal stream stays the
//     only source of truth and the Q face is a pure read-on-demand.
//   - ZERO emit points: the plugin never dispatches a frame or mutates fold
//     internals; the only store call is the exported write seam.
//   - It never touches store internals: the pull lives here, not in the store.
//
// The card body is the retired `info-panel.tsx` (now `./info-card.tsx`).

import { useEffect, useRef, useState, type ReactNode } from 'react';

import { getConversationInfo } from '../../api/client';
import { t } from '../../lib/i18n';
import { toggleOverlay, useOverlayOpen } from '../../lib/ui-overlays';
import {
	useCommittedCount,
	useThreadState,
} from '../../state/hooks';
import { store } from '../../state/bridge';
import { inject, register } from '../../state/slots';
import '../../slots.registry';
import { InfoPanel } from './info-card';

/** The Q-face refresh debounce: coalesce a burst of committed edges into one
 * pull (mirrors the reference plugin and the retired store scheduler). */
const INFO_DEBOUNCE_MS = 120;

/** The info-card overlay flag id (client view state, §F.2). */
const INFO_OVERLAY = 'conversation-info';

/**
 * Edge-triggered Q-face pull for one session: re-runs whenever `committed`
 * advances (exactly when spend/context numbers can change — not on streaming
 * deltas, not on a timer), debounced, skipped while the tab is hidden and
 * flushed when it becomes visible again. Writes the result through the
 * store's public `setConversationInfo` seam.
 */
function useConversationInfoPull(sessionId: string, active: boolean): void {
	const committed = useCommittedCount(sessionId);
	useEffect(() => {
		if (!sessionId) return undefined;
		let alive = true;
		let timer: ReturnType<typeof setTimeout> | undefined;
		const doFetch = (): void => {
			if (!alive) return;
			if (typeof document !== 'undefined' && document.hidden) {
				// Deferred: the visibility listener re-arms the pull when the
				// tab returns (never dropped).
				return;
			}
			void getConversationInfo(sessionId).then((info) => {
				if (alive && info) store.setConversationInfo(sessionId, info);
			});
		};
		const schedule = (): void => {
			if (timer !== undefined) clearTimeout(timer);
			timer = setTimeout(doFetch, INFO_DEBOUNCE_MS);
		};
		const onVisibility = (): void => {
			if (typeof document === 'undefined' || !document.hidden) {
				if (timer !== undefined) {
					clearTimeout(timer);
					timer = undefined;
				}
				doFetch();
			}
		};
		// Pull immediately when the card is open / a committed edge advanced;
		// the very first open also warms the payload (active gate).
		schedule();
		if (typeof document !== 'undefined') {
			document.addEventListener('visibilitychange', onVisibility);
		}
		return () => {
			alive = false;
			if (timer !== undefined) clearTimeout(timer);
			if (typeof document !== 'undefined') {
				document.removeEventListener('visibilitychange', onVisibility);
			}
		};
		// `committed` is the edge; `active` (card open) decides whether we
		// keep the payload warm. Re-runs on either change.
	}, [sessionId, committed, active]);
}

/**
 * The `conversation.session.header.utilities` occupant: a status chip that
 * toggles the info card. The pull runs for the life of the header (warming
 * the payload even before the card opens so the numbers are fresh on first
 * click), matching the reference plugin's always-on edge subscription.
 */
const ConversationInfoEntry = ({
	sessionId,
	models,
}: {
	sessionId: string;
	models: Parameters<typeof InfoPanel>[0]['models'];
}): ReactNode => {
	const open = useOverlayOpen(INFO_OVERLAY);
	const thread = useThreadState(sessionId);
	// Edge-triggered Q-face pull for this session (the payload is parked on
	// `thread.conversationInfo` via the store write seam and `InfoPanel`
	// renders it through the same selector — no local mirror).
	useConversationInfoPull(sessionId, /* active */ true);
	if (!thread) return null;
	return (
		<>
			<button
				className="text-muted-foreground hover:bg-accent flex size-6 cursor-pointer items-center justify-center rounded transition-colors"
				onClick={() => toggleOverlay(INFO_OVERLAY)}
				title={t('conversation_info')}
				type="button"
			>
				<InfoDot />
			</button>
			{open && (
				<div className="fixed right-4 top-14 z-20 flex max-h-[72vh] flex-col overflow-y-auto">
					<InfoPanel models={models} thread={thread} />
				</div>
			)}
		</>
	);
};

/** Small status dot mirroring the reference chip's live indicator. */
const InfoDot = (): ReactNode => (
	<svg
		aria-hidden
		className="size-4"
		fill="none"
		stroke="currentColor"
		strokeWidth={1.8}
		viewBox="0 0 24 24"
	>
		<circle cx="12" cy="12" r="9" />
		<path d="M12 8h.01M11 12h1v4h1" strokeLinecap="round" strokeLinejoin="round" />
	</svg>
);

// Contribution: run the registration for each declaration lifetime of the
// header-utilities slot (so a header that mounts/remounts never loses the
// plugin's chip, and the plugin never imports the header).
inject('conversation.session.header.utilities', () =>
	register(
		{
			name: 'conversation.session.header.utilities',
			id: 'conversation-info',
			order: 20,
			priority: 0,
			registrant: 'conversation-info',
		},
		ConversationInfoEntry,
	),
);
