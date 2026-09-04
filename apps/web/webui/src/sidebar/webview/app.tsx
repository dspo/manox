// Shell: switches between the thread list (home) and the active
// conversation. Thread states keep accumulating in the store regardless of
// which view is shown, so switching never interrupts or loses a running
// turn.

import { useEffect } from 'react';

import { api } from './api/client';
import { ConversationView } from './components/conversation-view';
import { ThreadsView } from './components/threads-view';
import { useChatState } from './state/bridge';

export const App = () => {
  const state = useChatState();
  const thread = state.activeThreadId ? (state.perThread[state.activeThreadId] ?? null) : null;

  // Global registries (models, threads, slash entries) load once on mount;
  // the host pushes updates afterwards.
  useEffect(() => {
    api.requestModels();
    api.listThreads();
    api.listCommands();
  }, []);

  // Spend/context (§E.3) is store-driven (committed-message edge → debounced
  // `GetConversationInfo`); the old turn-falling-edge usage refresh is a
  // §D.6 dead path and no longer lives here.

  if (state.view === 'conversation' && thread) {
    return (
      <ConversationView
        commands={state.commands}
        error={thread.error ?? state.error}
        models={state.models}
        thread={thread}
        threads={state.threads}
      />
    );
  }
  return (
    <ThreadsView
      commands={state.commands}
      error={state.error}
      models={state.models}
      threads={state.threads}
    />
  );
};
