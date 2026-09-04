// SessionManager behaviour against an in-memory transport: event routing,
// command sequencing, and the awaitOn timeout path.

import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('vscode', () => ({
  workspace: {
    workspaceFolders: undefined,
    getConfiguration: () => ({ get: () => undefined }),
  },
}));

import type { FromClient, FromServer } from '../dist/protocol';
import { SessionManager } from './sessionManager';
import type { Transport } from './transport/transport';

class FakeTransport implements Transport {
  readonly ready = Promise.resolve();
  sent: string[] = [];
  private handler: ((ev: FromServer) => void) | null = null;

  onEvent(handler: (ev: FromServer) => void): () => void {
    this.handler = handler;
    return () => {
      this.handler = null;
    };
  }

  send(command: string): void {
    this.sent.push(command);
  }

  async dispose(): Promise<void> {}

  emit(ev: FromServer): void {
    this.handler?.(ev);
  }

  lastCommand(): FromClient {
    return JSON.parse(this.sent[this.sent.length - 1]) as FromClient;
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

/** Helper: make a FromServer::Notification with the given method and fields. */
function note(method: string, fields: Record<string, unknown> = {}): FromServer {
  return { kind: 'notification', note: { method, ...fields } as never } as FromServer;
}

describe('init', () => {
  it('waits for the ready notification before resolving, once per process', async () => {
    const { transport, manager } = create();
    const pending = manager.init('/w');
    // init waits for the transport to be ready, then waits for the ready
    // notification from the server.
    await Promise.resolve();
    transport.emit(note('ready'));
    await pending;

    await manager.init('/other');
    // No additional commands sent on re-init.
    expect(transport.sent).toHaveLength(0);
  });

  it('rejects when the server never reports ready', async () => {
    vi.useFakeTimers();
    const { manager } = create();
    const pending = manager.init('/w');
    const assertion = expect(pending).rejects.toThrow(/timed out waiting for actor event: init ready/);
    await vi.advanceTimersByTimeAsync(30_001);
    await assertion;
  });
});

describe('createSession', () => {
  it('reuses a caller-supplied session id', async () => {
    const { transport, manager } = create();
    const pending = manager.createSession('/w', 'draft-1');
    const cmd = transport.lastCommand();
    expect(cmd).toEqual({
      kind: 'notification',
      note: { method: 'createSession', sessionId: 'draft-1', cwd: '/w' },
    });
    transport.emit(note('sessionCreated', { sessionId: 'draft-1' }));
    await expect(pending).resolves.toBe('draft-1');
  });

  it('resolves on sessionCreated and enforces the configured approval mode', async () => {
    const { transport, manager } = create();
    const pending = manager.createSession('/w');
    const cmd = transport.lastCommand();
    expect(cmd).toMatchObject({
      kind: 'notification',
      note: { method: 'createSession' },
    });
    const sessionId = (cmd as { note: { sessionId: string } }).note.sessionId;
    transport.emit(note('sessionCreated', { sessionId }));

    const resolved = await pending;
    expect(resolved).toBe(sessionId);
    // Unset config falls back to workspace-write; the host enforces it right away.
    expect(transport.lastCommand()).toEqual({
      kind: 'notification',
      note: { method: 'setApprovalMode', sessionId, mode: 'workspace-write' },
    });
  });

  it('rejects and reclaims the server-side session when the server never confirms', async () => {
    vi.useFakeTimers();
    const { transport, manager } = create();
    const pending = manager.createSession('/w');
    const cmd = transport.lastCommand();
    const sessionId = (cmd as { note: { sessionId: string } }).note.sessionId;
    const assertion = expect(pending).rejects.toThrow(
      /timed out waiting for actor event: session_created/,
    );
    await vi.advanceTimersByTimeAsync(5_001);
    await assertion;
    // The timeout path disposes the server-side session so a late
    // createSession cannot leave an orphaned thread behind.
    expect(transport.lastCommand()).toEqual({
      kind: 'notification',
      note: { method: 'disposeSession', sessionId },
    });
  });
});

describe('event routing', () => {
  it('delivers session events to the owning session only', async () => {
    const { transport, manager } = create();
    const pending = manager.createSession('/w');
    const cmd = transport.lastCommand();
    const sessionId = (cmd as { note: { sessionId: string } }).note.sessionId;
    transport.emit(note('sessionCreated', { sessionId }));
    await pending;

    const received: Record<string, unknown>[] = [];
    manager.onSessionEvent(sessionId, (ev) => received.push(ev));
    transport.emit(note('agentText', { sessionId, text: 'hi' }));
    transport.emit(note('agentText', { sessionId: 'other', text: 'nope' }));
    expect(received).toHaveLength(1);
    expect(received[0]).toMatchObject({ method: 'agentText', text: 'hi' });
  });

  it('keeps the emitter alive through the dispose confirmation', async () => {
    const { transport, manager } = create();
    const pending = manager.createSession('/w');
    const cmd = transport.lastCommand();
    const sessionId = (cmd as { note: { sessionId: string } }).note.sessionId;
    transport.emit(note('sessionCreated', { sessionId }));
    await pending;

    const received: Record<string, unknown>[] = [];
    manager.onSessionEvent(sessionId, (ev) => received.push(ev));
    manager.disposeSession(sessionId);
    expect(transport.lastCommand()).toEqual({
      kind: 'notification',
      note: { method: 'disposeSession', sessionId },
    });
    transport.emit(note('sessionDisposed', { sessionId }));
    transport.emit(note('agentText', { sessionId, text: 'late' }));
    expect(received.map((ev) => ev.method)).toEqual(['sessionDisposed']);
  });

  it('delivers untagged events to global subscribers', () => {
    const { transport, manager } = create();
    const received: Record<string, unknown>[] = [];
    const unsubscribe = manager.onGlobalEvent((ev) => received.push(ev));
    transport.emit(note('error', { message: 'boom' }));
    transport.emit(note('models', { models: [] }));
    expect(received.map((ev) => ev.method)).toEqual(['error', 'models']);
    unsubscribe();
    transport.emit(note('error', { message: 'after unsubscribe' }));
    expect(received).toHaveLength(2);
  });
});

describe('listModels', () => {
  it('resolves with the models payload', async () => {
    const { transport, manager } = create();
    const pending = manager.listModels();
    const cmd = transport.lastCommand();
    expect(cmd).toMatchObject({
      kind: 'request',
      call: { method: 'listModels' },
    });
    transport.emit(note('models', {
      models: [{ id: 'm', name: 'M', provider: 'p', api: 'anthropic', context_window: 200000 }],
    }));
    await expect(pending).resolves.toEqual([
      { id: 'm', name: 'M', provider: 'p', api: 'anthropic', context_window: 200000 },
    ]);
  });

  it('rejects after the registration budget', async () => {
    vi.useFakeTimers();
    const { manager } = create();
    const pending = manager.listModels();
    const assertion = expect(pending).rejects.toThrow(/timed out waiting for actor event: models/);
    await vi.advanceTimersByTimeAsync(30_001);
    await assertion;
  });
});

describe('openThread', () => {
  it('resolves on sessionCreated without overriding the persisted approval mode', async () => {
    const { transport, manager } = create();
    const pending = manager.openThread('t1');
    const cmd = transport.lastCommand();
    expect(cmd).toMatchObject({
      kind: 'request',
      call: { method: 'openSession', sessionId: 't1' },
    });
    transport.emit(note('sessionCreated', { sessionId: 't1' }));
    await expect(pending).resolves.toBe('t1');
    // Restored threads keep their persisted policy — no setApprovalMode.
    expect(transport.sent).toHaveLength(1);
  });

  it('rejects and reclaims the server-side session when the server never confirms', async () => {
    vi.useFakeTimers();
    const { transport, manager } = create();
    const pending = manager.openThread('t1');
    const assertion = expect(pending).rejects.toThrow(
      /timed out waiting for actor event: session_created/,
    );
    await vi.advanceTimersByTimeAsync(5_001);
    await assertion;
    expect(transport.lastCommand()).toEqual({
      kind: 'notification',
      note: { method: 'disposeSession', sessionId: 't1' },
    });
  });
});

describe('listThreads', () => {
  it('resolves with the threads payload', async () => {
    const { transport, manager } = create();
    const pending = manager.listThreads();
    expect(transport.lastCommand()).toMatchObject({
      kind: 'request',
      call: { method: 'listThreads' },
    });
    transport.emit(note('threadsUpdated', {
      threads: [
        {
          id: 't1',
          title: 'Fix the bug',
          updated_at: 1_700_000_000,
          running: false,
          unread: true,
          errored: false,
          pending_auth: false,
          pending_plan: false,
          background_work: false,
          pinned: false,
          archived: false,
          parent_id: null,
          depth: 0,
          model_id: 'm',
        },
      ],
    }));
    await expect(pending).resolves.toEqual([
      expect.objectContaining({ id: 't1', title: 'Fix the bug', unread: true }),
    ]);
  });
});

describe('archiveThread / pinThread', () => {
  it('sends the store-mutation commands through the transport', () => {
    const { transport, manager } = create();
    manager.archiveThread('t1', true);
    expect(transport.lastCommand()).toEqual({
      kind: 'notification',
      note: { method: 'archiveThread', sessionId: 't1', archived: true },
    });
    manager.archiveThread('t1', false);
    expect(transport.lastCommand()).toEqual({
      kind: 'notification',
      note: { method: 'archiveThread', sessionId: 't1', archived: false },
    });
    manager.pinThread('t2', true);
    expect(transport.lastCommand()).toEqual({
      kind: 'notification',
      note: { method: 'pinThread', sessionId: 't2', pinned: true },
    });
  });
});

describe('listCommands', () => {
  it('resolves with the commands payload', async () => {
    const { transport, manager } = create();
    const pending = manager.listCommands();
    expect(transport.lastCommand()).toMatchObject({
      kind: 'request',
      call: { method: 'listCommands' },
    });
    transport.emit(note('commands', {
      commands: [
        { name: 'deliver', description: 'Ship it', kind: 'command', argument_hint: null },
      ],
    }));
    await expect(pending).resolves.toEqual([
      expect.objectContaining({ name: 'deliver', kind: 'command' }),
    ]);
  });
});

  // T10c: the requestThreadInfo suite retired with the method (§D.6 — the
  // doomed `threadInfo` note surface; successor §E.3 GetConversationInfo).

describe('setApprovalMode', () => {
  it('broadcasts the policy to every live session', async () => {
    const { transport, manager } = create();
    const pending = manager.createSession('/w');
    const cmd = transport.lastCommand();
    const sessionId = (cmd as { note: { sessionId: string } }).note.sessionId;
    transport.emit(note('sessionCreated', { sessionId }));
    await pending;

    manager.setApprovalMode('danger-full-access');
    expect(transport.lastCommand()).toEqual({
      kind: 'notification',
      note: { method: 'setApprovalMode', sessionId, mode: 'danger-full-access' },
    });
  });
});

// ── T9: v2 frames through the host ───────────────────────────────────────

/** Helper: a full ThreadListItem for mirror assertions. */
function thread(id: string, fields: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    id,
    title: `t-${id}`,
    updated_at: 1,
    running: false,
    unread: false,
    errored: false,
    pending_auth: false,
    pending_plan: false,
    background_work: false,
    model_id: 'm',
    pinned: false,
    archived: false,
    parent_id: null,
    depth: 0,
    ...fields,
  };
}

/** Helper: a FromServer::Host frame. */
function host(hostEvent: Record<string, unknown>): FromServer {
  return { kind: 'host', host: hostEvent } as FromServer;
}

describe('v2 frames (T9)', () => {
  it('onFrame relays every envelope kind unfiltered, in arrival order', () => {
    const { transport, manager } = create();
    const seen: FromServer[] = [];
    const unsubscribe = manager.onFrame((msg) => seen.push(msg));

    const frames = [
      { kind: 'streamItem', streamId: 'st-1', frame: { type: 'entry', seq: 1 } },
      { kind: 'streamEnd', streamId: 'st-1', reason: { type: 'closed' } },
      host({ type: 'sessionStatus', sessionId: 's1' }),
      { kind: 'response', id: 'x-1', outcome: { Ok: { accepted: true } } },
      { kind: 'request', id: 'a-1', call: { method: 'approve', sessionId: 's1' } },
      note('agentText', { sessionId: 's1', text: 'hi' }),
    ] as unknown as FromServer[];
    for (const frame of frames) transport.emit(frame);

    expect(seen).toEqual(frames);
    unsubscribe();
    transport.emit(frames[0]);
    expect(seen).toHaveLength(frames.length);
  });

  it('init resolves on the v2 host-ready frame (no legacy note needed)', async () => {
    const { transport, manager } = create();
    const pending = manager.init('/w');
    await Promise.resolve();
    transport.emit(host({ type: 'ready', epoch: 1 }));
    await expect(pending).resolves.toBeUndefined();
  });

  it('listModels resolves from the v2 host-models frame', async () => {
    const { transport, manager } = create();
    const pending = manager.listModels();
    transport.emit(host({ type: 'models', models: [{ id: 'm', name: 'M', provider: 'p', api: 'anthropic', context_window: 1 }] }));
    await expect(pending).resolves.toHaveLength(1);
  });

  it('listThreads resolves from the §D.5 mirror, still refreshing via request', async () => {
    const { transport, manager } = create();
    transport.emit(host({ type: 'threadsUpdated', threads: [thread('t1')] }));
    const pending = manager.listThreads();
    // The request is still sent (the server answers with a fresh list note);
    // the await short-circuits from the mirror.
    expect(transport.lastCommand()).toMatchObject({
      kind: 'request',
      call: { method: 'listThreads' },
    });
    await expect(pending).resolves.toEqual([expect.objectContaining({ id: 't1' })]);
  });

  it('sessionStatus deltas seed and merge the thread mirror', async () => {
    const { transport, manager } = create();
    transport.emit(
      host({
        type: 'sessionStatus',
        sessionId: 's1',
        running: true,
        errored: null,
        unread: null,
        pendingAuth: null,
        pendingPlan: null,
        backgroundWork: null,
      }),
    );
    const seeded = await manager.listThreads();
    expect(seeded).toEqual([expect.objectContaining({ id: 's1', running: true })]);

    // A full snapshot replaces the mirror; a later status delta merges into it.
    transport.emit(host({ type: 'threadsUpdated', threads: [thread('s1'), thread('s2')] }));
    transport.emit(host({ type: 'sessionStatus', sessionId: 's2', running: true }));
    const merged = await manager.listThreads();
    expect(merged).toEqual([
      expect.objectContaining({ id: 's1', title: 't-s1' }),
      expect.objectContaining({ id: 's2', title: 't-s2', running: true }),
    ]);
  });

  it('legacy threadsUpdated notes keep the mirror in sync', async () => {
    const { transport, manager } = create();
    manager.listThreads(); // prime: no snapshot yet, await registered
    transport.emit(note('threadsUpdated', { threads: [thread('t9')] }));
    await expect(manager.listThreads()).resolves.toEqual([
      expect.objectContaining({ id: 't9' }),
    ]);
  });

  it('unknown host variants and frames never throw', () => {
    const { transport, manager } = create();
    const received: Record<string, unknown>[] = [];
    manager.onGlobalEvent((ev) => received.push(ev));
    transport.emit(host({ type: 'someFutureEvent' }));
    transport.emit({ kind: 'streamItem', streamId: 's', frame: { type: 'quantumLeap' } } as unknown as FromServer);
    expect(received).toHaveLength(0);
  });
});