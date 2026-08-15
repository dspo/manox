// Store behaviour: per-thread event routing, tool-card folding, restored
// history mapping, and the view/thread bookkeeping around them.

import { describe, expect, it, vi } from 'vitest';

import type { ActorEvent, WireMessage } from '../../../protocol';
import type { HostToWebview } from '../../messages';
import { Store, wireMessagesToTranscriptItems } from './store';
import { foldToolStatus } from './transcript';

const event = (ev: ActorEvent): HostToWebview => ({ type: 'event', event: ev });

const ready = (sessionId: string, kind: 'fresh' | 'restored' = 'fresh'): HostToWebview => ({
  type: 'session_ready',
  sessionId,
  cwd: '/w',
  kind,
});

const startSession = (sessionId = 's', kind: 'fresh' | 'restored' = 'fresh'): Store => {
  const store = new Store();
  store.dispatch(ready(sessionId, kind));
  return store;
};

const thread = (store: Store, id = 's') => store.get().perThread[id];

const toolCard = (store: Store, id: string, sessionId = 's') => {
  const item = thread(store, sessionId)?.items.find((i) => i.kind === 'tool' && i.id === id);
  return item && item.kind === 'tool' ? item.tool : undefined;
};

describe('foldToolStatus', () => {
  it('folds terminal wire statuses into UI semantics', () => {
    expect(foldToolStatus('success')).toBe('completed');
    expect(foldToolStatus('error')).toBe('failed');
  });

  it('passes authorization and progress statuses through', () => {
    for (const status of ['pending-approval', 'running', 'denied', 'cancelled', 'continued'] as const) {
      expect(foldToolStatus(status)).toBe(status);
    }
  });

  it('treats unknown statuses as running', () => {
    expect(foldToolStatus('some-new-status')).toBe('running');
  });
});

describe('thread routing', () => {
  it('session_ready opens the conversation view with a fresh thread state', () => {
    const store = startSession();
    const state = store.get();
    expect(state.view).toBe('conversation');
    expect(state.activeThreadId).toBe('s');
    expect(thread(store)).toMatchObject({
      sessionId: 's',
      cwd: '/w',
      items: [],
      loading: false,
      // Matches the thread-side default; restored threads get their
      // persisted mode replayed by the actor.
      approvalMode: 'autopilot',
    });
  });

  it('restored sessions start in the loading state', () => {
    const store = startSession('s', 'restored');
    expect(thread(store, 's')?.loading).toBe(true);
  });

  it('routes events to their own thread only', () => {
    const store = startSession('a');
    store.dispatch(ready('b'));
    store.dispatch(event({ type: 'agent_text', sessionId: 'a', text: 'to a' }));
    store.dispatch(event({ type: 'agent_text', sessionId: 'b', text: 'to b' }));
    expect(thread(store, 'a')?.items).toHaveLength(1);
    expect(thread(store, 'b')?.items).toHaveLength(1);
    expect(thread(store, 'a')?.items[0]).toMatchObject({ text: 'to a' });
    // The last session_ready wins the active slot.
    expect(store.get().activeThreadId).toBe('b');
  });

  it('keeps accumulating hidden threads after switching away', () => {
    const store = startSession('a');
    store.dispatch(ready('b'));
    store.dispatch(event({ type: 'agent_text', sessionId: 'a', text: 'background' }));
    expect(store.get().activeThreadId).toBe('b');
    expect(thread(store, 'a')?.items[0]).toMatchObject({ text: 'background' });
  });

  it('session_disposed drops the thread and falls back to the list', () => {
    const store = startSession('a');
    store.dispatch(event({ type: 'session_disposed', sessionId: 'a' }));
    expect(store.get().perThread.a).toBeUndefined();
    expect(store.get().view).toBe('threads');
    expect(store.get().activeThreadId).toBeNull();
  });

  it('openLocal and backToList switch views without touching thread state', () => {
    const store = startSession('a');
    store.dispatch(event({ type: 'agent_text', sessionId: 'a', text: 'x' }));
    store.backToList();
    expect(store.get().view).toBe('threads');
    store.openLocal('a');
    expect(store.get().view).toBe('conversation');
    expect(store.get().activeThreadId).toBe('a');
    expect(thread(store, 'a')?.items).toHaveLength(1);
  });
});

describe('transcript folding', () => {
  it('streams text into the trailing item of the same kind', () => {
    const store = startSession();
    store.dispatch(event({ type: 'agent_text', sessionId: 's', text: 'Hel' }));
    store.dispatch(event({ type: 'agent_text', sessionId: 's', text: 'lo' }));
    expect(thread(store)?.items).toEqual([
      expect.objectContaining({ kind: 'assistant', text: 'Hello' }),
    ]);

    store.dispatch(event({ type: 'agent_thinking', sessionId: 's', text: 'hm' }));
    store.dispatch(event({ type: 'agent_thinking', sessionId: 's', text: 'm' }));
    store.dispatch(event({ type: 'agent_text', sessionId: 's', text: '!' }));
    const items = thread(store)?.items ?? [];
    expect(items).toHaveLength(3);
    expect(items[1]).toMatchObject({ kind: 'thinking', text: 'hmm' });
  });

  it('stamps assistant items with the model the turn started with', () => {
    const store = startSession();
    store.dispatch(event({ type: 'current_model', sessionId: 's', id: 'm1' }));
    store.dispatch(event({ type: 'turn_started', sessionId: 's' }));
    store.dispatch(event({ type: 'model_changed', sessionId: 's', from: 'm1', to: 'm2' }));
    store.dispatch(event({ type: 'agent_text', sessionId: 's', text: 'hi' }));
    const item = thread(store)?.items[0];
    expect(item).toMatchObject({ kind: 'assistant', modelId: 'm1' });
  });

  it('inserts tool cards with folded wire status', () => {
    const store = startSession();
    store.dispatch(
      event({
        type: 'tool_call',
        sessionId: 's',
        id: 't1',
        name: 'bash',
        title: 'ls',
        status: 'pending-approval',
      }),
    );
    expect(toolCard(store, 't1')?.status).toBe('pending-approval');

    store.dispatch(
      event({
        type: 'tool_call',
        sessionId: 's',
        id: 't1',
        name: 'bash',
        title: 'ls -la',
        status: 'success',
      }),
    );
    const card = toolCard(store, 't1');
    expect(card?.status).toBe('completed');
    expect(card?.title).toBe('ls -la');
    expect(thread(store)?.items.filter((i) => i.kind === 'tool')).toHaveLength(1);
  });

  it('records live output and the final result', () => {
    const store = startSession();
    store.dispatch(
      event({ type: 'tool_call', sessionId: 's', id: 't1', name: 'bash', title: 'ls', status: 'running' }),
    );
    store.dispatch(event({ type: 'tool_output', sessionId: 's', id: 't1', chunk: 'a\n' }));
    store.dispatch(event({ type: 'tool_output', sessionId: 's', id: 't1', chunk: 'b\n' }));
    expect(toolCard(store, 't1')?.output).toBe('a\nb\n');

    store.dispatch(
      event({ type: 'tool_result', sessionId: 's', id: 't1', output: 'done', is_error: false }),
    );
    const card = toolCard(store, 't1');
    expect(card?.output).toBe('done');
    expect(card?.isError).toBe(false);
    expect(card?.status).toBe('completed');
  });

  it('marks failed results even if the wire status lagged', () => {
    const store = startSession();
    store.dispatch(
      event({ type: 'tool_call', sessionId: 's', id: 't1', name: 'bash', title: 'ls', status: 'running' }),
    );
    store.dispatch(
      event({ type: 'tool_result', sessionId: 's', id: 't1', output: 'boom', is_error: true }),
    );
    expect(toolCard(store, 't1')?.status).toBe('failed');
    expect(toolCard(store, 't1')?.isError).toBe(true);
  });

  it('adds authorization cards and removes them on decision', () => {
    const store = startSession();
    store.dispatch(
      event({
        type: 'tool_call_authorization',
        sessionId: 's',
        id: 'a1',
        tool_name: 'bash',
        summary: 'run rm',
        input: { cmd: 'rm' },
      }),
    );
    expect(thread(store)?.items.find((i) => i.kind === 'approval')).toMatchObject({
      id: 'a1',
      toolName: 'bash',
    });

    store.decideApproval('s', 'a1');
    expect(thread(store)?.items.find((i) => i.kind === 'approval')).toBeUndefined();
  });

  it('drives the turn flag', () => {
    const store = startSession();
    store.dispatch(event({ type: 'turn_started', sessionId: 's' }));
    expect(thread(store)?.turnActive).toBe(true);
    store.dispatch(
      event({ type: 'turn_finished', sessionId: 's', cancelled: false, failed: false }),
    );
    expect(thread(store)?.turnActive).toBe(false);
  });

  it('times turns for the meta line', () => {
    const store = startSession();
    const now = vi.spyOn(Date, 'now');
    now.mockReturnValue(100_000);
    store.dispatch(event({ type: 'turn_started', sessionId: 's' }));
    expect(thread(store)?.turnStartedAt).toBe(100_000);
    now.mockReturnValue(103_000);
    store.dispatch(
      event({ type: 'turn_finished', sessionId: 's', cancelled: false, failed: false }),
    );
    expect(thread(store)?.turnStartedAt).toBeNull();
    expect(thread(store)?.lastTurnDurationSec).toBe(3);
    now.mockRestore();
  });

  it('routes session errors to their own thread only', () => {
    const store = startSession('a');
    store.dispatch(ready('b'));
    store.dispatch(event({ type: 'error', sessionId: 'a', message: 'boom' }));
    // A background thread's failure must not surface in another
    // conversation's banner.
    expect(thread(store, 'a')?.error).toBe('boom');
    expect(thread(store, 'b')?.error).toBeNull();
    expect(store.get().error).toBeNull();
  });

  it('clears a thread error when its next turn starts', () => {
    const store = startSession('a');
    store.dispatch(event({ type: 'error', sessionId: 'a', message: 'boom' }));
    store.dispatch(event({ type: 'turn_started', sessionId: 'a' }));
    expect(thread(store, 'a')?.error).toBeNull();
  });

  it('drops session errors for unknown sessions instead of spawning ghost threads', () => {
    const store = startSession('a');
    store.dispatch(event({ type: 'error', sessionId: 'ghost', message: 'boom' }));
    expect(Object.keys(store.get().perThread)).toEqual(['a']);
    expect(store.get().error).toBeNull();
  });

  it('keeps session-less errors global and clears them on session_ready', () => {
    const store = new Store();
    store.dispatch(event({ type: 'error', sessionId: null, message: 'core hiccup' }));
    expect(store.get().error).toBe('core hiccup');
    store.dispatch(ready('s'));
    expect(store.get().error).toBeNull();
  });

  it('tracks model, approval-mode, and usage changes per thread', () => {
    const store = startSession();
    store.dispatch(event({ type: 'model_changed', sessionId: 's', from: null, to: 'm1' }));
    expect(thread(store)?.currentModelId).toBe('m1');
    store.dispatch(event({ type: 'current_model', sessionId: 's', id: 'm2' }));
    expect(thread(store)?.currentModelId).toBe('m2');
    store.dispatch(event({ type: 'approval_mode_changed', sessionId: 's', mode: 'autopilot' }));
    expect(thread(store)?.approvalMode).toBe('autopilot');
    store.dispatch(
      event({ type: 'usage', sessionId: 's', usage: { input_tokens: 1 }, cost: 0.5 }),
    );
    expect(thread(store)?.usage).toEqual({ input_tokens: 1 });
    expect(thread(store)?.cost).toBe(0.5);
  });

  it('echoUser stamps the current model and wall-clock time', () => {
    const store = startSession();
    store.dispatch(event({ type: 'current_model', sessionId: 's', id: 'm1' }));
    store.echoUser('s', 'hi', [{ mimeType: 'image/png', data: 'data:img', byteLen: null }]);
    const item = thread(store)?.items[0];
    expect(item).toMatchObject({
      kind: 'user',
      text: 'hi',
      modelId: 'm1',
      images: [{ mimeType: 'image/png', data: 'data:img', byteLen: null }],
    });
    if (item?.kind === 'user') {
      expect(typeof item.timestamp).toBe('number');
    }
  });
});

describe('global folds', () => {
  it('stores models, commands, and surfaces errors', () => {
    const store = new Store();
    store.dispatch({
      type: 'models',
      models: [{ id: 'm', name: 'M', provider: 'p', api: 'anthropic', context_window: 200000 }],
    });
    store.dispatch({
      type: 'commands',
      commands: [{ name: 'deliver', description: 'Ship', kind: 'command', argument_hint: null }],
    });
    expect(store.get().models).toHaveLength(1);
    expect(store.get().commands).toHaveLength(1);
    store.dispatch({ type: 'global_error', message: 'core down' });
    expect(store.get().error).toBe('core down');
  });

  it('threads snapshots sync titles into live thread states', () => {
    const store = startSession('t1');
    store.dispatch({
      type: 'threads',
      threads: [
        {
          id: 't1',
          title: 'Fix the bug',
          updated_at: 1,
          running: false,
          unread: false,
          errored: false,
          pending_auth: false,
          model_id: 'm',
        },
      ],
    });
    expect(store.get().threads).toHaveLength(1);
    expect(thread(store, 't1')?.title).toBe('Fix the bug');
  });

  it('thread_info, branch, and plan events land on the thread', () => {
    const store = startSession();
    store.dispatch({
      type: 'thread_info',
      sessionId: 's',
      info: {
        worktree_path: '/w',
        plan: null,
        usage: {},
        cost: 1,
        pending_auth_count: 0,
        agents: [],
      },
    });
    expect(thread(store)?.info?.worktree_path).toBe('/w');

    store.dispatch(event({ type: 'branch', sessionId: 's', branch: 'main' }));
    expect(thread(store)?.branch).toBe('main');

    store.dispatch(
      event({
        type: 'plan_updated',
        sessionId: 's',
        snapshot: { explanation: null, steps: [{ step: 'a', status: 'pending' }] },
      }),
    );
    expect(thread(store)?.info?.plan?.steps).toHaveLength(1);

    store.dispatch(event({ type: 'worktree_changed', sessionId: 's', active: false, path: null }));
    expect(thread(store)?.info?.worktree_path).toBeNull();
  });

  it('carries per-model usage in thread_info and merges async git_stats', () => {
    const store = startSession();
    store.dispatch({
      type: 'thread_info',
      sessionId: 's',
      info: {
        worktree_path: null,
        plan: null,
        usage: {},
        per_model_usage: { 'anthropic/claude-x': { input_tokens: 10, output_tokens: 4 } },
        cost: 0,
        pending_auth_count: 0,
        agents: [],
      },
    });
    expect(thread(store)?.info?.per_model_usage).toEqual({
      'anthropic/claude-x': { input_tokens: 10, output_tokens: 4 },
    });

    store.dispatch(
      event({ type: 'git_stats', sessionId: 's', stats: { added: 3, deleted: 1, untracked: 2 } }),
    );
    const info = thread(store)?.info;
    expect(info?.git_stats).toEqual({ added: 3, deleted: 1, untracked: 2 });
    expect(info?.per_model_usage).toBeDefined();
  });

  it('aggregates sub-agent start and progress into the info snapshot', () => {
    const store = startSession();
    store.dispatch(
      event({
        type: 'subagent_started',
        sessionId: 's',
        id: 'ag1',
        agent_type: 'explorer',
        description: 'find things',
      }),
    );
    store.dispatch(
      event({
        type: 'subagent_progress',
        sessionId: 's',
        id: 'ag1',
        agent_type: 'explorer',
        tool_uses: 3,
        latest_activity: 'grep',
        status: 'success',
      }),
    );
    expect(thread(store)?.info?.agents).toEqual([
      expect.objectContaining({ id: 'ag1', tool_uses: 3, latest_activity: 'grep', status: 'success' }),
    ]);
  });
});

describe('wireMessagesToTranscriptItems', () => {
  const wire = (partial: Partial<WireMessage> & { id: string }): WireMessage => ({
    timestamp: 1_700_000_000,
    parent_id: null,
    provenance: 'assistant',
    role: 'assistant',
    content: [],
    ...partial,
  });

  it('maps user text with ui metadata and deflated images', () => {
    const items = wireMessagesToTranscriptItems([
      wire({
        id: 'u1',
        role: 'user',
        provenance: 'user',
        content: [
          { Text: 'look at this' },
          { Image: { mime_type: 'image/png', byte_len: 1234 } },
        ],
        ui: { model_id: 'm1', display_text: '/deliver now' },
      }),
    ]);
    expect(items).toEqual([
      {
        kind: 'user',
        id: 'u1',
        text: 'look at this',
        displayText: '/deliver now',
        modelId: 'm1',
        timestamp: 1_700_000_000,
        images: [{ mimeType: 'image/png', data: null, byteLen: 1234 }],
      },
    ]);
  });

  it('skips goal provenance and empty user messages', () => {
    const items = wireMessagesToTranscriptItems([
      wire({ id: 'g1', role: 'user', provenance: 'goal_continuation', content: [{ Text: 'x' }] }),
      wire({ id: 'u2', role: 'user', provenance: 'user', content: [] }),
    ]);
    expect(items).toEqual([]);
  });

  it('maps assistant text, thinking, tool use, and compaction blocks', () => {
    const items = wireMessagesToTranscriptItems([
      wire({
        id: 'a1',
        content: [
          { Thinking: { text: 'ponder', signature: null } },
          { Text: 'answer' },
          {
            ToolUse: {
              id: 'tu1',
              name: 'bash',
              raw_input: '{"cmd":"ls"}',
              input: { cmd: 'ls' },
              is_input_complete: true,
              thought_signature: null,
            },
          },
          { Compaction: 'earlier context' },
        ],
        ui: { model_id: 'm1' },
      }),
    ]);
    expect(items.map((i) => i.kind)).toEqual(['thinking', 'assistant', 'tool', 'compaction']);
    expect(items[1]).toMatchObject({ kind: 'assistant', text: 'answer', modelId: 'm1' });
    expect(items[2]).toMatchObject({
      kind: 'tool',
      id: 'tu1',
      tool: expect.objectContaining({ name: 'bash', status: 'completed' }),
    });
  });

  it('pairs tool results with their tool use and marks errors', () => {
    const items = wireMessagesToTranscriptItems([
      wire({
        id: 'a1',
        content: [
          {
            ToolUse: {
              id: 'tu1',
              name: 'bash',
              raw_input: '{}',
              input: {},
              is_input_complete: true,
              thought_signature: null,
            },
          },
        ],
      }),
      wire({
        id: 't1',
        role: 'system',
        provenance: 'tool',
        content: [
          { ToolResult: { tool_use_id: 'tu1', tool_name: 'bash', is_error: true, content: 'boom' } },
        ],
      }),
    ]);
    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({
      kind: 'tool',
      tool: expect.objectContaining({ status: 'failed', output: 'boom', isError: true }),
    });
  });

  it('creates standalone tool items for unmatched results', () => {
    const items = wireMessagesToTranscriptItems([
      wire({
        id: 't1',
        role: 'system',
        provenance: 'tool',
        content: [
          { ToolResult: { tool_use_id: 'nope', tool_name: 'read', is_error: false, content: 'ok' } },
        ],
      }),
    ]);
    expect(items).toEqual([
      expect.objectContaining({
        kind: 'tool',
        tool: expect.objectContaining({ id: 'nope', name: 'read', status: 'completed' }),
      }),
    ]);
  });
});

describe('Store notifications', () => {
  it('notifies subscribers on dispatch and echoUser', () => {
    const store = new Store();
    let calls = 0;
    const unsubscribe = store.subscribe(() => calls++);
    store.dispatch(ready('s'));
    store.echoUser('s', 'hi');
    expect(calls).toBe(2);
    unsubscribe();
    store.dispatch(event({ type: 'agent_text', sessionId: 's', text: 'x' }));
    expect(calls).toBe(2);
  });

  it('skips notification when a fold changes nothing', () => {
    const store = startSession();
    let calls = 0;
    store.subscribe(() => calls++);
    store.dispatch(event({ type: 'token_usage', sessionId: 's', input: 1, output: 2, cache_creation: 0, cache_read: 0 }));
    expect(calls).toBe(0);
  });
});
