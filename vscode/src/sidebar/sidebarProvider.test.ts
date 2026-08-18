// Sidebar host batching: session events accumulate in a per-frame buffer
// and cross the webview bridge as one `events` message; bypass messages
// drain the buffer first so the wire order matches arrival order.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import * as vscode from 'vscode';

import type { ThreadInfoSnapshot } from '../protocol';
import type { HostToWebview, WebviewToHost } from './messages';
import { registerManoxSidebar } from './sidebarProvider';

const { managerMock } = vi.hoisted(() => {
  const handlers = new Map<string, (ev: unknown) => void>();
  const managerMock = {
    handlers,
    sent: [] as Record<string, unknown>[],
    init: vi.fn(async () => {}),
    createSession: vi.fn(async (_cwd: string, id?: string) => id ?? 'generated'),
    onSessionEvent(sessionId: string, handler: (ev: unknown) => void): () => void {
      handlers.set(sessionId, handler);
      return () => handlers.delete(sessionId);
    },
    onGlobalEvent: vi.fn(() => () => {}),
    archiveThread: vi.fn(),
    send(command: Record<string, unknown>): void {
      this.sent.push(command);
    },
    disposeSession: vi.fn(),
    emit(sessionId: string, ev: unknown): void {
      handlers.get(sessionId)?.(ev);
    },
  };
  return { managerMock };
});

vi.mock('vscode', () => ({
  window: { registerWebviewViewProvider: vi.fn() },
  commands: { registerCommand: vi.fn(), executeCommand: vi.fn() },
  Uri: { joinPath: (...parts: unknown[]) => parts.join('/') },
  env: { language: 'en' },
}));

vi.mock('../sessionManager', () => ({
  SessionManager: { shared: () => managerMock },
  resolveWorkspaceCwd: () => '/w',
}));

interface Harness {
  provider: { newSession(opts?: { sessionId?: string }): Promise<void> };
  posted: HostToWebview[];
  disposeView: () => void;
  onMessage: (msg: WebviewToHost) => void;
}

const setup = (): Harness => {
  const context = { extensionUri: 'ext', subscriptions: [] as { dispose(): void }[] };
  registerManoxSidebar(context as never);
  const register = vscode.window.registerWebviewViewProvider as unknown as ReturnType<
    typeof vi.fn
  >;
  const provider = register.mock.calls.at(-1)?.[1] as {
    resolveWebviewView(view: unknown): void;
    newSession(opts?: { sessionId?: string }): Promise<void>;
  };
  const posted: HostToWebview[] = [];
  let disposeView = () => {};
  let onMessage: (msg: WebviewToHost) => void = () => {};
  provider.resolveWebviewView({
    webview: {
      options: {},
      html: '',
      cspSource: 'csp',
      asWebviewUri: (uri: unknown) => uri,
      postMessage: (message: HostToWebview) => {
        posted.push(message);
        return Promise.resolve(true);
      },
      onDidReceiveMessage: (cb: (msg: WebviewToHost) => void) => {
        onMessage = cb;
        return { dispose() {} };
      },
    },
    onDidDispose: (cb: () => void) => {
      disposeView = cb;
      return { dispose() {} };
    },
  });
  return { provider, posted, disposeView, onMessage };
};

const threadInfo: ThreadInfoSnapshot = {
  reasoning_effort: 'high',
  worktree_path: null,
  plan: null,
  goal: null,
  usage: {},
  cost: 0,
  pending_auth_count: 0,
  agents: [],
};

beforeEach(() => {
  vi.useFakeTimers();
  managerMock.handlers.clear();
  managerMock.sent.length = 0;
  managerMock.disposeSession.mockClear();
  managerMock.createSession.mockClear();
  managerMock.archiveThread.mockClear();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('sidebar event batching', () => {
  it('posts session_ready before any buffered event of that session', async () => {
    const { provider, posted } = setup();
    await provider.newSession({ sessionId: 's1' });
    expect(managerMock.sent.at(-1)).toMatchObject({ cmd: 'get_current_model', sessionId: 's1' });
    managerMock.emit('s1', { type: 'agent_text', sessionId: 's1', text: 'a' });
    vi.advanceTimersByTime(33);
    expect(posted.map((m) => m.type)).toEqual(['session_ready', 'events']);
  });

  it('coalesces a frame of events into one events message in arrival order', async () => {
    const { provider, posted } = setup();
    await provider.newSession({ sessionId: 's1' });
    managerMock.emit('s1', { type: 'agent_text', sessionId: 's1', text: 'a' });
    managerMock.emit('s1', { type: 'agent_text', sessionId: 's1', text: 'b' });
    expect(posted).toHaveLength(1);
    vi.advanceTimersByTime(33);
    expect(posted).toHaveLength(2);
    expect(posted[1]).toEqual({
      type: 'events',
      events: [
        { type: 'agent_text', sessionId: 's1', text: 'a' },
        { type: 'agent_text', sessionId: 's1', text: 'b' },
      ],
    });
  });

  it('drains the buffer before a thread_info bypass so it never overtakes events', async () => {
    const { provider, posted } = setup();
    await provider.newSession({ sessionId: 's1' });
    managerMock.emit('s1', { type: 'agent_text', sessionId: 's1', text: 'x' });
    managerMock.emit('s1', { type: 'thread_info', sessionId: 's1', info: threadInfo });
    expect(posted.map((m) => m.type)).toEqual(['session_ready', 'events', 'thread_info']);
  });

  it('flushes buffered events before re-announcing an already-live session', async () => {
    const { provider, posted } = setup();
    await provider.newSession({ sessionId: 's1' });
    managerMock.emit('s1', { type: 'agent_text', sessionId: 's1', text: 'x' });
    await (provider as unknown as { openThread(id: string): Promise<void> }).openThread('s1');
    expect(posted.map((m) => m.type)).toEqual(['session_ready', 'events', 'session_ready']);
    expect(posted[2]).toMatchObject({ kind: 'restored' });
  });

  it('drops the buffer and timer on teardown without posting', async () => {
    const { provider, posted, disposeView } = setup();
    await provider.newSession({ sessionId: 's1' });
    managerMock.emit('s1', { type: 'agent_text', sessionId: 's1', text: 'x' });
    disposeView();
    vi.advanceTimersByTime(100);
    expect(posted.map((m) => m.type)).toEqual(['session_ready']);
    expect(managerMock.disposeSession).toHaveBeenCalledWith('s1');
  });
});

describe('plan_execute_fresh', () => {
  const message: WebviewToHost = {
    type: 'plan_execute_fresh',
    sessionId: 's1',
    planFile: '/p/plan.md',
    cwd: '/w',
  };

  // Both awaited steps of the orchestration IIFE resolve immediately; a few
  // microtask hops drain it deterministically under fake timers.
  const settle = async (): Promise<void> => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  };

  it('registers the fresh session before seeding execution', async () => {
    const { posted, onMessage } = setup();
    onMessage(message);
    await settle();

    const seed = managerMock.sent.find((c) => c.cmd === 'plan_seed_execution') as
      | { cmd: string; sessionId: string; planFile: string }
      | undefined;
    expect(seed).toBeDefined();
    const freshId = seed!.sessionId;
    expect(managerMock.sent.map((c) => c.cmd)).toEqual([
      'get_current_model',
      'plan_seed_execution',
    ]);
    expect(managerMock.archiveThread).toHaveBeenCalledWith('s1', true);
    expect(managerMock.createSession).toHaveBeenCalledWith('/w', freshId);
    expect(posted).toEqual([
      { type: 'session_ready', sessionId: freshId, cwd: '/w', kind: 'fresh' },
    ]);
    expect(managerMock.handlers.has(freshId)).toBe(true);

    // The core symptom regression: the fresh session's events reach the
    // webview without the user manually opening the thread.
    managerMock.emit(freshId, { type: 'agent_text', sessionId: freshId, text: 'seed' });
    vi.advanceTimersByTime(33);
    expect(posted.at(-1)).toEqual({
      type: 'events',
      events: [{ type: 'agent_text', sessionId: freshId, text: 'seed' }],
    });
  });

  it('disposes the orphaned session when teardown wins the create race', async () => {
    const { posted, onMessage, disposeView } = setup();
    let releaseInit!: () => void;
    managerMock.init.mockReturnValueOnce(
      new Promise<void>((resolve) => {
        releaseInit = resolve;
      }),
    );
    onMessage(message);
    disposeView();
    releaseInit();
    await settle();

    const freshId = managerMock.createSession.mock.calls.at(-1)?.[1];
    expect(freshId).toBeDefined();
    expect(managerMock.disposeSession).toHaveBeenCalledWith(freshId);
    expect(managerMock.sent.some((c) => c.cmd === 'plan_seed_execution')).toBe(false);
    expect(posted).toEqual([]);
  });
});
