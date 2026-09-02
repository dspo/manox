// Sidebar host: a transparent typed relay. `FromClient` from the webview
// forwards to the manager; `ServerNote` notifications and `ServerCall`
// requests from the manager forward to the webview verbatim (no batching,
// no legacy `session_ready` — the store derives readiness from
// `SessionCreated`). Host-only lifecycle verbs orchestrate the manager.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import * as vscode from 'vscode';

import type { FromClient, FromServer } from '../../dist/protocol';
import type { ToHost, ToWebview } from '../../dist/sidebar/messages';
import { registerManoxSidebar } from './sidebarProvider';

const { managerMock } = vi.hoisted(() => {
  const handlers = new Map<string, (ev: Record<string, unknown>) => void>();
  const callHandlers = new Map<string, (ev: { id: string; call: Record<string, unknown> }) => void>();
  const managerMock = {
    handlers,
    callHandlers,
    sent: [] as Record<string, unknown>[],
    init: vi.fn(async () => {}),
    createSession: vi.fn(async (_cwd: string, id?: string) => id ?? 'generated'),
    openThread: vi.fn(async (id: string) => id),
    onSessionEvent(sessionId: string, handler: (ev: Record<string, unknown>) => void): () => void {
      handlers.set(sessionId, handler);
      return () => handlers.delete(sessionId);
    },
    onSessionServerCall(
      sessionId: string,
      handler: (ev: { id: string; call: Record<string, unknown> }) => void,
    ): () => void {
      callHandlers.set(sessionId, handler);
      return () => callHandlers.delete(sessionId);
    },
    onGlobalEvent: vi.fn(() => () => {}),
    send(msg: Record<string, unknown>): void {
      this.sent.push(msg);
    },
    disposeSession: vi.fn(),
    emit(sessionId: string, ev: Record<string, unknown>): void {
      handlers.get(sessionId)?.(ev);
    },
    emitCall(sessionId: string, ev: { id: string; call: Record<string, unknown> }): void {
      callHandlers.get(sessionId)?.(ev);
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
  posted: ToWebview[];
  disposeView: () => void;
  onMessage: (msg: ToHost) => void;
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

  const posted: ToWebview[] = [];
  let disposeView = () => {};
  let onMessage: (msg: ToHost) => void = () => {};
  provider.resolveWebviewView({
    webview: {
      options: {},
      html: '',
      cspSource: 'csp',
      asWebviewUri: (uri: unknown) => uri,
      postMessage: (message: ToWebview) => {
        posted.push(message);
        return Promise.resolve(true);
      },
      onDidReceiveMessage: (cb: (msg: ToHost) => void) => {
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

beforeEach(() => {
  vi.useFakeTimers();
  managerMock.handlers.clear();
  managerMock.callHandlers.clear();
  managerMock.sent.length = 0;
  managerMock.disposeSession.mockClear();
  managerMock.createSession.mockClear();
  managerMock.openThread.mockClear();
});

afterEach(() => {
  vi.useRealTimers();
});

const isNote = (m: ToWebview): m is FromServer =>
  typeof m === 'object' && m !== null && 'kind' in m && (m as FromServer).kind !== undefined;

describe('transparent relay', () => {
  it('newSession posts a synthetic SessionCreated and forwards events individually', async () => {
    const { provider, posted } = setup();
    await provider.newSession({ sessionId: 's1' });
    // Synthetic readiness (the manager's await already consumed the real one).
    expect(posted).toContainEqual({
      kind: 'notification',
      note: { method: 'sessionCreated', sessionId: 's1' },
    });
    // No batching: each ServerNote forwards as its own frame.
    managerMock.emit('s1', { method: 'agentText', sessionId: 's1', text: 'a' });
    managerMock.emit('s1', { method: 'agentText', sessionId: 's1', text: 'b' });
    expect(posted.filter(isNote).at(-1)).toEqual({
      kind: 'notification',
      note: { method: 'agentText', sessionId: 's1', text: 'b' },
    });
  });

  it('forwards ServerCall requests as FromServer::Request frames', async () => {
    const { provider, posted } = setup();
    await provider.newSession({ sessionId: 's1' });
    managerMock.emitCall('s1', {
      id: 'auth1',
      call: { method: 'approve', sessionId: 's1', authId: 'auth1' },
    });
    expect(posted).toContainEqual({
      kind: 'request',
      id: 'auth1',
      call: { method: 'approve', sessionId: 's1', authId: 'auth1' },
    });
  });

  it('forwards FromClient from the webview verbatim to the manager', async () => {
    const { onMessage } = setup();
    const submit: FromClient = {
      kind: 'notification',
      note: { method: 'submit', sessionId: 's1', text: 'hi', images: [], clientId: null },
    };
    onMessage(submit);
    expect(managerMock.sent).toContainEqual(submit);
  });

  it('teardown disposes every live session and unsubscribes', async () => {
    const { provider, posted, disposeView } = setup();
    await provider.newSession({ sessionId: 's1' });
    disposeView();
    expect(managerMock.disposeSession).toHaveBeenCalledWith('s1');
    // A late event after teardown does not reach the (disposed) webview.
    managerMock.emit('s1', { method: 'agentText', sessionId: 's1', text: 'late' });
    expect(posted.at(-1)).not.toEqual({
      kind: 'notification',
      note: { method: 'agentText', sessionId: 's1', text: 'late' },
    });
  });
});

describe('plan_execute_fresh', () => {
  const message: ToHost = {
    kind: 'plan_execute_fresh',
    sessionId: 's1',
    planFile: '/p/plan.md',
    cwd: '/w',
  } as ToHost;

  // The orchestration IIFE's awaited steps resolve immediately; a few
  // microtask hops drain it deterministically under fake timers.
  const settle = async (): Promise<void> => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  };

  it('archives, creates, registers, and seeds the fresh session', async () => {
    const { posted, onMessage } = setup();
    onMessage(message);
    await settle();

    const seed = managerMock.sent.find(
      (c) =>
        (c as { note?: { method?: string } })?.note?.method === 'planSeedExecution',
    ) as { note: { method: string; sessionId: string; planFile: string } } | undefined;
    expect(seed).toBeDefined();
    const freshId = seed!.note.sessionId;
    expect(managerMock.createSession).toHaveBeenCalledWith('/w', freshId);
    expect(managerMock.sent).toContainEqual({
      kind: 'notification',
      note: { method: 'archiveThread', sessionId: 's1', archived: true },
    });
    // Synthetic readiness for the fresh session reached the webview.
    expect(posted).toContainEqual({
      kind: 'notification',
      note: { method: 'sessionCreated', sessionId: freshId },
    });
    // The fresh session's events forward without a manual open.
    managerMock.emit(freshId, { method: 'agentText', sessionId: freshId, text: 'seed' });
    expect(posted.at(-1)).toEqual({
      kind: 'notification',
      note: { method: 'agentText', sessionId: freshId, text: 'seed' },
    });
  });

  it('disposes the orphaned session when teardown wins the create race', async () => {
    const { onMessage, disposeView } = setup();
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

    expect(managerMock.createSession).toHaveBeenCalledTimes(1);
    const freshId = managerMock.createSession.mock.calls[0][1];
    expect(managerMock.disposeSession).toHaveBeenCalledWith(freshId);
  });
});
