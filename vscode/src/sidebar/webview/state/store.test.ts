import { describe, expect, it } from 'vitest';

import type { ActorEvent } from '../../../protocol';
import type { HostToWebview } from '../../messages';
import { Store, initialState, normalizeToolStatus, reduce } from './store';

const event = (ev: ActorEvent): HostToWebview => ({ type: 'event', event: ev });

const toolCard = (state: ReturnType<typeof initialState>, id: string) => {
  const item = state.items.find((i) => i.kind === 'tool' && i.call.id === id);
  return item && item.kind === 'tool' ? item.call : undefined;
};

describe('normalizeToolStatus', () => {
  it('folds terminal wire statuses into UI semantics', () => {
    expect(normalizeToolStatus('success')).toBe('completed');
    expect(normalizeToolStatus('error')).toBe('failed');
  });

  it('passes authorization and progress statuses through', () => {
    for (const status of ['pending-approval', 'running', 'denied', 'cancelled', 'continued']) {
      expect(normalizeToolStatus(status)).toBe(status);
    }
  });
});

describe('reduce', () => {
  it('session_ready resets state and carries the new identity', () => {
    const busy = reduce(initialState(), event({ type: 'turn_started', sessionId: 'old' }));
    const next = reduce(busy, { type: 'session_ready', sessionId: 's2', cwd: '/w' });
    expect(next).toEqual({ ...initialState(), sessionId: 's2', cwd: '/w' });
  });

  it('streams text into the trailing item of the same kind', () => {
    let s = reduce(initialState(), event({ type: 'agent_text', sessionId: 's', text: 'Hel' }));
    s = reduce(s, event({ type: 'agent_text', sessionId: 's', text: 'lo' }));
    expect(s.items).toEqual([{ kind: 'assistant', text: 'Hello' }]);

    s = reduce(s, event({ type: 'agent_thinking', sessionId: 's', text: 'hm' }));
    expect(s.items).toHaveLength(2);
    expect(s.items[1]).toEqual({ kind: 'thinking', text: 'hm' });

    s = reduce(s, event({ type: 'agent_text', sessionId: 's', text: '!' }));
    expect(s.items).toHaveLength(3);
  });

  it('inserts tool cards with normalized wire status', () => {
    let s = reduce(
      initialState(),
      event({
        type: 'tool_call',
        sessionId: 's',
        id: 't1',
        name: 'bash',
        title: 'ls',
        status: 'pending-approval',
      }),
    );
    expect(toolCard(s, 't1')?.status).toBe('pending-approval');

    s = reduce(
      s,
      event({
        type: 'tool_call',
        sessionId: 's',
        id: 't1',
        name: 'bash',
        title: 'ls -la',
        status: 'success',
      }),
    );
    const card = toolCard(s, 't1');
    expect(card?.status).toBe('completed');
    expect(card?.title).toBe('ls -la');
    expect(s.items.filter((i) => i.kind === 'tool')).toHaveLength(1);
  });

  it('records live output and the final result', () => {
    let s = reduce(
      initialState(),
      event({
        type: 'tool_call',
        sessionId: 's',
        id: 't1',
        name: 'bash',
        title: 'ls',
        status: 'running',
      }),
    );
    s = reduce(s, event({ type: 'tool_output', sessionId: 's', id: 't1', chunk: 'a\n' }));
    s = reduce(s, event({ type: 'tool_output', sessionId: 's', id: 't1', chunk: 'b\n' }));
    expect(toolCard(s, 't1')?.output).toBe('a\nb\n');

    s = reduce(
      s,
      event({ type: 'tool_result', sessionId: 's', id: 't1', output: 'done', is_error: false }),
    );
    expect(toolCard(s, 't1')?.result).toEqual({ output: 'done', isError: false });
  });

  it('marks failed results even if the wire status lagged', () => {
    let s = reduce(
      initialState(),
      event({
        type: 'tool_call',
        sessionId: 's',
        id: 't1',
        name: 'bash',
        title: 'ls',
        status: 'running',
      }),
    );
    s = reduce(
      s,
      event({ type: 'tool_result', sessionId: 's', id: 't1', output: 'boom', is_error: true }),
    );
    expect(toolCard(s, 't1')?.status).toBe('failed');
  });

  it('tracks authorization cards through the decision', () => {
    let s = reduce(
      initialState(),
      event({
        type: 'tool_call_authorization',
        sessionId: 's',
        id: 'a1',
        tool_name: 'bash',
        summary: 'run rm',
        input: { cmd: 'rm' },
      }),
    );
    const card = s.items.find((i) => i.kind === 'approval');
    expect(card).toMatchObject({ kind: 'approval', id: 'a1', decided: null });

    const store = new Store();
    store.dispatch(event({
      type: 'tool_call_authorization',
      sessionId: 's',
      id: 'a1',
      tool_name: 'bash',
      summary: 'run rm',
      input: {},
    }));
    store.decideApproval('a1', false);
    const decided = store.get().items.find((i) => i.kind === 'approval');
    expect(decided).toMatchObject({ decided: 'denied' });
  });

  it('drives the turn flag and clears errors on turn start', () => {
    let s = reduce(initialState(), event({ type: 'error', sessionId: 's', message: 'bad' }));
    expect(s.error).toBe('bad');
    s = reduce(s, event({ type: 'turn_started', sessionId: 's' }));
    expect(s.turnActive).toBe(true);
    expect(s.error).toBeNull();
    s = reduce(s, event({ type: 'turn_finished', sessionId: 's', cancelled: false, failed: false }));
    expect(s.turnActive).toBe(false);
  });

  it('tracks model and approval-mode changes', () => {
    let s = reduce(
      initialState(),
      event({ type: 'model_changed', sessionId: 's', from: null, to: 'm1' }),
    );
    expect(s.currentModelId).toBe('m1');
    s = reduce(s, event({ type: 'current_model', sessionId: 's', id: 'm2' }));
    expect(s.currentModelId).toBe('m2');
    s = reduce(s, event({ type: 'approval_mode_changed', sessionId: 's', mode: 'danger' }));
    expect(s.approvalMode).toBe('danger');
  });

  it('maps live token_usage events onto the usage snapshot', () => {
    const s = reduce(
      initialState(),
      event({
        type: 'token_usage',
        sessionId: 's',
        input: 10,
        output: 20,
        cache_creation: 30,
        cache_read: 40,
      }),
    );
    expect(s.usage).toEqual({
      input_tokens: 10,
      output_tokens: 20,
      cache_creation_input_tokens: 30,
      cache_read_input_tokens: 40,
    });
  });

  it('replaces the usage snapshot wholesale on a get_usage reply', () => {
    let s = reduce(
      initialState(),
      event({
        type: 'token_usage',
        sessionId: 's',
        input: 10,
        output: 20,
        cache_creation: 30,
        cache_read: 40,
      }),
    );
    s = reduce(s, event({ type: 'usage', sessionId: 's', usage: { input_tokens: 1 } }));
    expect(s.usage).toEqual({ input_tokens: 1 });
  });

  it('stores models and surfaces errors', () => {
    let s = reduce(initialState(), {
      type: 'models',
      models: [{ id: 'm', name: 'M', provider: 'p' }],
    });
    expect(s.models).toHaveLength(1);
    s = reduce(s, { type: 'global_error', message: 'core down' });
    expect(s.error).toBe('core down');
  });
});

describe('Store', () => {
  it('notifies subscribers on dispatch and echoUser', () => {
    const store = new Store();
    let calls = 0;
    const unsubscribe = store.subscribe(() => calls++);
    store.echoUser('hi');
    expect(store.get().items).toEqual([{ kind: 'user', text: 'hi' }]);
    store.dispatch({ type: 'session_reset' });
    expect(calls).toBe(2);
    unsubscribe();
    store.dispatch({ type: 'session_reset' });
    expect(calls).toBe(2);
  });
});
