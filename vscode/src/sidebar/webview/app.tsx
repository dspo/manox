// Shell: top bar, transcript, error banner, composer, usage — stacked in a
// full-height column; the turn flag falling edge requests a usage snapshot.

import { useEffect, useRef } from 'react';

import { api } from './api/client';
import { Composer } from './components/chrome/composer';
import { ErrorBanner } from './components/chrome/error-banner';
import { ModelPicker } from './components/chrome/model-picker';
import { TopBar } from './components/chrome/top-bar';
import { UsageBar } from './components/chrome/usage-bar';
import { MessageList } from './components/transcript/message-list';
import { useChatState } from './state/bridge';

export const App = () => {
  const state = useChatState();
  const prevTurnActive = useRef(state.turnActive);

  useEffect(() => {
    if (prevTurnActive.current && !state.turnActive) {
      api.requestUsage();
    }
    prevTurnActive.current = state.turnActive;
  }, [state.turnActive]);

  return (
    <div className="font-chrome flex h-screen flex-col bg-background text-foreground">
      <TopBar approvalMode={state.approvalMode}>
        <ModelPicker
          currentModelId={state.currentModelId}
          disabled={state.turnActive || state.models.length === 0}
          models={state.models}
        />
      </TopBar>
      <MessageList items={state.items} turnActive={state.turnActive} />
      <ErrorBanner message={state.error} />
      <Composer ready={state.sessionId !== null} turnActive={state.turnActive} />
      <UsageBar usage={state.usage} />
    </div>
  );
};
