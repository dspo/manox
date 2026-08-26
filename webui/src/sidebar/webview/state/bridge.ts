// Session bridge: one shared store folds host messages into chat state, and
// React components observe it through useSyncExternalStore.

import { useSyncExternalStore } from 'react';

import { onHostMessage } from '../api/client';
import type { ChatState } from './store';
import { Store } from './store';

export const store = new Store();

onHostMessage((msg) => store.dispatch(msg));

const subscribe = (listener: () => void) => store.subscribe(listener);
const getSnapshot = () => store.get();

export function useChatState(): ChatState {
  return useSyncExternalStore(subscribe, getSnapshot);
}

export type { ChatState, ThreadState, ToolCallState, TranscriptItem, UserImage } from './store';
