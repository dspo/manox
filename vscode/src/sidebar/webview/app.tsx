// Shell: switches between the thread list (home) and the active
// conversation. Thread states keep accumulating in the store regardless of
// which view is shown, so switching never interrupts or loses a running
// turn.

import { useEffect, useRef } from 'react';

import { api, ThreadApi } from './api/client';
import { ConversationView } from './components/conversation-view';
import { ThreadsView } from './components/threads-view';
import { useChatState } from './state/bridge';

export const App = () => {
  const state = useChatState();
  const thread = state.activeThreadId ? (state.perThread[state.activeThreadId] ?? null) : null;
  const turnActive = thread?.turnActive ?? false;
  const prevTurnActive = useRef(false);

  // Global registries (models, threads, slash entries) load once on mount;
  // the host pushes updates afterwards.
  useEffect(() => {
    api.requestModels();
    api.listThreads();
    api.listCommands();
  }, []);

  // The active turn flag's falling edge refreshes usage and the info
  // snapshot (spend tree, git stats) for the finished turn.
  useEffect(() => {
    if (prevTurnActive.current && !turnActive && thread) {
      const threadApi = new ThreadApi(thread.sessionId);
      threadApi.requestUsage();
      threadApi.requestThreadInfo();
    }
    prevTurnActive.current = turnActive;
  }, [turnActive, thread]);

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
