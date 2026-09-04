// Shell: switches between the thread list (home) and the active
// conversation. Thread states keep accumulating in the store regardless of
// which view is shown, so switching never interrupts or loses a running
// turn.

import { useEffect } from 'react';

import { api } from './api/client';
import { ConversationView } from './components/conversation-view';
import { ThreadsView } from './components/threads-view';
import { useChatState } from './state/bridge';
import { Slot } from './slots.outlet';

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

  // Spend/context (§E.3) is no longer store-driven: the conversation-info
  // plugin (T8 §H) watches the `committed` edge and pulls `GetConversationInfo`
  // through its own seam. App keeps no request effects (the old turn-falling-
  // edge usage refresh was a §D.6 dead path and is gone).

  const body =
    state.view === 'conversation' && thread ? (
      <ConversationView
        commands={state.commands}
        error={thread.error ?? state.error}
        models={state.models}
        thread={thread}
        threads={state.threads}
      />
    ) : (
      <ThreadsView
        commands={state.commands}
        error={state.error}
        models={state.models}
        threads={state.threads}
      />
    );

  return (
    <>
      {body}
      {/* App-level modal overlays (§G): the settings sheet and any plugin
       * overlay render here through the `shell.overlay` outlet. */}
      <Slot name="shell.overlay" owner={{ sessionId: state.activeThreadId }} />
    </>
  );
};
