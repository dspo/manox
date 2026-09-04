// Session bridge: one shared store folds `FromServer` frames into chat
// state. The api client installs the transport effects (stream paging,
// history pages) when it connects the store. Read access for components
// goes through `state/hooks.ts` selectors (L9); the `store` export carries
// the write/command surface used by the view layer.
//
// §E.3 visibility awareness moved with the conversation-info pull into the
// conversation-info plugin (T8 §H): the store keeps only the durable
// committed-message edge signal.

import { connectStore } from '../api/client';
import { Store } from './store';

export const store = new Store();

connectStore(store);

export { useChatState } from './hooks';
export type { ChatState, ThreadState, ToolCallState, TranscriptItem, UserImage } from './store';
