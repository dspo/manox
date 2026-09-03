// Session bridge: one shared store folds `FromServer` messages into chat
// state, and React components observe it through useSyncExternalStore.

import { useSyncExternalStore } from 'react';

import { connectStore } from '../api/client';
import type { ChatState } from './store';
import { Store } from './store';

export const store = new Store();

connectStore(store);

const subscribe = (listener: () => void) => store.subscribe(listener);
const getSnapshot = () => store.get();

export function useChatState(): ChatState {
  return useSyncExternalStore(subscribe, getSnapshot);
}

export type { ChatState, ThreadState, ToolCallState, TranscriptItem, UserImage } from './store';
