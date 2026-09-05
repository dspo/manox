// Sidebar host: a transparent typed relay (T9). `FromClient` from the
// webview — including the v2 `streamOpen` / `streamCancel` frames — forwards
// to the manager verbatim; EVERY raw `FromServer` frame (notification /
// request / response / host / streamItem / streamEnd) forwards to the webview
// unfiltered: the host never intercepts ServerCall requests (the shared v2
// bundle answers them). Host-only lifecycle verbs orchestrate the manager.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import * as vscode from 'vscode';

import type { FromClient, FromServer } from '../../dist/protocol';
import type { ToHost, ToWebview } from '../../dist/sidebar/messages';
import { registerManoxSidebar } from './sidebarProvider';

const { managerMock } = vi.hoisted(() => {
  const frameHandlers = new Set<(msg: Record<string, unknown>) => void>();
  const managerMock = {
    frameHandlers,
    sent: [] as Record<string, unknown>[],
    init: vi.fn(async () => {}),
    createSession: vi.fn(async (_cwd: string, id?: string) => id ?? 'generated'),
    openThread: vi.fn(async (id: string) => id),
    /** T9 raw relay: the real manager hands every `FromServer` envelope to
     * these subscribers with zero filtering. */
    onFrame(handler: (msg: Record<string, unknown>) => void): () => void {
      frameHandlers.add(handler);
      return () => frameHandlers.delete(handler);
    },
    send(msg: Record<string, unknown>): void {
      this.sent.push(msg);
    },
    disposeSession: vi.fn(),
    emitFrame(msg: Record<string, unknown>): void {
      for (const handler of frameHandlers) handler(msg);
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
  managerMock.frameHandlers.clear();
  managerMock.sent.length = 0;
  managerMock.disposeSession.mockClear();
  managerMock.createSession.mockClear();
  managerMock.openThread.mockClear();
});

afterEach(() => {
  vi.useRealTimers();
});

const isFrame = (m: ToWebview): m is FromServer =>
  typeof m === 'object' && m !== null && 'kind' in m && (m as FromServer).kind !== undefined;

describe('transparent relay (T9 v2 frames)', () => {
  it('forwards every FromServer envelope verbatim, unfiltered', async () => {
    const { posted } = setup();
    const frames: FromServer[] = [
      { kind: 'notification', note: { method: 'sessionCreated', sessionId: 's1' } } as FromServer,
      {
        kind: 'streamItem',
        streamId: 'st-1',
        frame: { type: 'entry', seq: 1, event: { type: 'agentTextDelta', s: 'hi' } },
      } as unknown as FromServer,
      { kind: 'streamEnd', streamId: 'st-1', reason: { type: 'closed' } } as FromServer,
      {
        kind: 'host',
        host: { type: 'sessionStatus', sessionId: 's1', running: true },
      } as FromServer,
      {
        kind: 'request',
        id: 'auth1',
        call: { method: 'approve', sessionId: 's1', authId: 'auth1' },
      } as FromServer,
      { kind: 'response', id: 'webui-1', outcome: { Ok: { accepted: true } } } as FromServer,
    ];
    for (const frame of frames) managerMock.emitFrame(frame);
    // The host does not need any session registered for a frame to flow, and
    // it does not intercept ServerCall `request` or `response` frames: the
    // shared v2 bundle in the webview correlates and answers them.
    expect(posted.filter(isFrame)).toEqual(frames);
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

  it('forwards the v2 streamOpen / streamCancel frames to the manager', () => {
    const { onMessage } = setup();
    const open = {
      kind: 'streamOpen',
      streamId: 'webui-st',
      streamKind: { type: 'followSession', sessionId: 's1', maxMessages: null },
    } as FromClient;
    const cancel = { kind: 'streamCancel', streamId: 'webui-st' } as FromClient;
    onMessage(open as unknown as ToHost);
    onMessage(cancel as unknown as ToHost);
    expect(managerMock.sent).toContainEqual(open);
    expect(managerMock.sent).toContainEqual(cancel);
  });

  it('teardown disposes every live session and detaches the relay', async () => {
    const { provider, posted, disposeView } = setup();
    await provider.newSession({ sessionId: 's1' });
    disposeView();
    expect(managerMock.disposeSession).toHaveBeenCalledWith('s1');
    expect(managerMock.frameHandlers.size).toBe(0);
    // A late frame after teardown does not reach the (disposed) webview.
    managerMock.emitFrame({ kind: 'notification', note: { method: 'ready' } });
    expect(posted.at(-1)).not.toEqual({ kind: 'notification', note: { method: 'ready' } });
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

  it('archives, creates, and seeds the fresh session; frames flow via the relay', async () => {
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
    // No synthetic readiness: the server's real `sessionCreated` reaches the
    // webview through the raw relay like any other frame.
    managerMock.emitFrame({
      kind: 'notification',
      note: { method: 'sessionCreated', sessionId: freshId },
    });
    expect(posted.at(-1)).toEqual({
      kind: 'notification',
      note: { method: 'sessionCreated', sessionId: freshId },
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
