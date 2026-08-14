import { ArrowUp, Square } from 'lucide-react';
import type { KeyboardEvent } from 'react';
import { useCallback, useState } from 'react';

import { api } from '../../api/client';
import { store } from '../../state/bridge';
import { Button } from '../ui/button';
import { Textarea } from '../ui/textarea';

export type ComposerProps = {
  turnActive: boolean;
  /** Session established and subscribed; input is collected but sending is
   * blocked until the host can actually deliver it. */
  ready: boolean;
};

export const Composer = ({ turnActive, ready }: ComposerProps) => {
  const [text, setText] = useState('');

  const submit = useCallback(() => {
    const trimmed = text.trim();
    if (!trimmed || turnActive || !ready) {
      return;
    }
    store.echoUser(trimmed);
    api.submit(trimmed);
    setText('');
  }, [text, turnActive, ready]);

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  };

  return (
    <div className="flex items-end gap-2 border-t p-2">
      <Textarea
        className="min-h-[52px] flex-1 resize-none font-transcript"
        onChange={(e) => setText(e.target.value)}
        onKeyDown={handleKeyDown}
        placeholder={ready ? 'Message manox' : 'Starting session…'}
        rows={2}
        value={text}
      />
      {turnActive ? (
        <Button onClick={() => api.cancel()} size="icon" title="Stop" variant="secondary">
          <Square className="size-4" />
        </Button>
      ) : (
        <Button
          disabled={!ready || !text.trim()}
          onClick={submit}
          size="icon"
          title="Send"
        >
          <ArrowUp className="size-4" />
        </Button>
      )}
    </div>
  );
};
