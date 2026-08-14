// SessionManager behaviour against an in-memory transport: event routing,
// command sequencing, and the awaitOn timeout path.

import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('vscode', () => ({
  workspace: {
    workspaceFolders: undefined,
    getConfiguration: () => ({ get: () => undefined }),
  },
}));

import type { ActorEvent } from './protocol';
import { SessionManager } from './sessionManager';
import type { Transport } from './transport/transport';

class FakeTransport implements Transport {
  readonly ready = Promise.resolve();
  sent: string[] = [];
  private handler: ((ev: ActorEvent) => void) | null = null;

  onEvent(handler: (ev: ActorEvent) => void): () => void {
    this.handler = handler;
    return () => {
      this.handler = null;
    };
  }

  send(command: string): void {
    this.sent.push(command);
  }

  async dispose(): Promise<void> {}

  emit(ev: ActorEvent): void {
    this.handler?.(ev);
  }

  lastCommand(): Record<string, unknown> {
    return JSON.parse(this.sent[this.sent.length - 1]) as Record<string, unknown>;
  }
}

/** The constructor is private (production code goes through `shared()`);
 * tests inject the transport directly. */
const create = () => {
  const transport = new FakeTransport();
  const manager = new (SessionManager as unknown as new (t: Transport) => SessionManager)(
    transport,
  );
  return { transport, manager };
};

afterEach(() => {
  vi.useRealTimers();
});

describe('init', () => {
  it('waits for the ready event before resolving, once per process', async () => {
    const { transport, manager } = create();
    const pending = manager.init('/w');
    // The init command goes out only after `transport.ready` settles.
    await Promise.resolve();
    expect(transport.lastCommand()).toEqual({ cmd: 'init', cwd: '/w' });
    transport.emit({ type: 'ready' });
    await pending;

    await manager.init('/other');
    expect(transport.sent).toHaveLength(1);
  });

  it('rejects when the actor never reports ready', async () => {
    vi.useFakeTimers();
    const { manager } = create();
    const pending = manager.init('/w');
    // Attach the assertion before advancing so the rejection is handled the
    // moment the timer fires.
    const assertion = expect(pending).rejects.toThrow(/timed out waiting for actor event: init ready/);
    await vi.advanceTimersByTimeAsync(30_001);
    await assertion;
  });
});

describe('createSession', () => {
  it('resolves on session_created and enforces the configured approval mode', async () => {
    const { transport, manager } = create();
    const pending = manager.createSession('/w');
    const createCmd = transport.lastCommand();
    expect(createCmd.cmd).toBe('create_session');
    transport.emit({ type: 'session_created', sessionId: createCmd.sessionId as string });

    const sessionId = await pending;
    expect(sessionId).toBe(createCmd.sessionId);
    // Unset config falls back to danger; the host enforces it right away.
    expect(transport.lastCommand()).toEqual({
      cmd: 'set_approval_mode',
      sessionId,
      mode: 'danger',
    });
  });

  it('rejects and cleans up when the actor never confirms', async () => {
    vi.useFakeTimers();
    const { transport, manager } = create();
    const pending = manager.createSession('/w');
    const assertion = expect(pending).rejects.toThrow(
      /timed out waiting for actor event: session_created/,
    );
    await vi.advanceTimersByTimeAsync(5_001);
    await assertion;
    expect(transport.lastCommand().cmd).toBe('create_session');
  });
});

describe('event routing', () => {
  it('delivers session events to the owning session only', async () => {
    const { transport, manager } = create();
    const pending = manager.createSession('/w');
    const sessionId = transport.lastCommand().sessionId as string;
    transport.emit({ type: 'session_created', sessionId });
    await pending;

    const received: ActorEvent[] = [];
    manager.onSessionEvent(sessionId, (ev) => received.push(ev));
    transport.emit({ type: 'agent_text', sessionId, text: 'hi' });
    transport.emit({ type: 'agent_text', sessionId: 'other', text: 'nope' });
    expect(received).toHaveLength(1);
    expect(received[0]).toMatchObject({ type: 'agent_text', text: 'hi' });
  });

  it('keeps the emitter alive through the dispose confirmation', async () => {
    const { transport, manager } = create();
    const pending = manager.createSession('/w');
    const sessionId = transport.lastCommand().sessionId as string;
    transport.emit({ type: 'session_created', sessionId });
    await pending;

    const received: ActorEvent[] = [];
    manager.onSessionEvent(sessionId, (ev) => received.push(ev));
    manager.disposeSession(sessionId);
    expect(transport.lastCommand()).toEqual({ cmd: 'dispose_session', sessionId });
    transport.emit({ type: 'session_disposed', sessionId });
    transport.emit({ type: 'agent_text', sessionId, text: 'late' });
    expect(received.map((ev) => ev.type)).toEqual(['session_disposed']);
  });

  it('delivers untagged events to global subscribers', () => {
    const { transport, manager } = create();
    const received: ActorEvent[] = [];
    const unsubscribe = manager.onGlobalEvent((ev) => received.push(ev));
    transport.emit({ type: 'error', message: 'boom' });
    transport.emit({ type: 'models', models: [] });
    expect(received.map((ev) => ev.type)).toEqual(['error', 'models']);
    unsubscribe();
    transport.emit({ type: 'error', message: 'after unsubscribe' });
    expect(received).toHaveLength(2);
  });
});

describe('listModels', () => {
  it('resolves with the models payload', async () => {
    const { transport, manager } = create();
    const pending = manager.listModels();
    expect(transport.lastCommand()).toEqual({ cmd: 'list_models' });
    transport.emit({ type: 'models', models: [{ id: 'm', name: 'M', provider: 'p' }] });
    await expect(pending).resolves.toEqual([{ id: 'm', name: 'M', provider: 'p' }]);
  });

  it('rejects after the response timeout', async () => {
    vi.useFakeTimers();
    const { manager } = create();
    const pending = manager.listModels();
    const assertion = expect(pending).rejects.toThrow(/timed out waiting for actor event: models/);
    await vi.advanceTimersByTimeAsync(5_001);
    await assertion;
  });
});

describe('setApprovalMode', () => {
  it('broadcasts the policy to every live session', async () => {
    const { transport, manager } = create();
    const pending = manager.createSession('/w');
    const sessionId = transport.lastCommand().sessionId as string;
    transport.emit({ type: 'session_created', sessionId });
    await pending;

    manager.setApprovalMode('autopilot');
    expect(transport.lastCommand()).toEqual({
      cmd: 'set_approval_mode',
      sessionId,
      mode: 'autopilot',
    });
  });
});
