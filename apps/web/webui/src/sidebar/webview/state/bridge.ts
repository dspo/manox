// Session bridge: one shared store folds `FromServer` frames into chat state,
// and React components observe it through useSyncExternalStore. The api
// client installs the transport effects (stream paging, conversation-info
// pull) when it connects the store.

import { useSyncExternalStore } from 'react';

import { connectStore } from '../api/client';
import type { ChatState } from './store';
import { Store } from './store';

export const store = new Store();

connectStore(store);

// §E.3 visibility awareness: a conversation-info pull that went dirty while
// the tab was hidden drains as soon as it becomes visible again.
if (typeof document !== 'undefined') {
	document.addEventListener('visibilitychange', () => {
		if (document.hidden) return;
		for (const sessionId of store.liveSessionIds()) store.flushConversationInfo(sessionId);
	});
}

const subscribe = (listener: () => void) => store.subscribe(listener);
const getSnapshot = () => store.get();

export function useChatState(): ChatState {
	return useSyncExternalStore(subscribe, getSnapshot);
}

export type { ChatState, ThreadState, ToolCallState, TranscriptItem, UserImage } from './store';
