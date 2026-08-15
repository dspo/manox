// ManoxModelProvider behaviour against an in-memory transport: model
// information mapping, session-key caching, delta/seed submission, thinking
// streaming, cancellation, eviction, and part serialization.

import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('vscode', () => {
  let approvalMode: string | undefined;
  let thinkingAvailable = true;

  class LanguageModelTextPart {
    constructor(public value: string) {}
  }
  class LanguageModelThinkingPart {
    constructor(
      public value: string,
      public id?: string,
      public metadata?: { readonly [key: string]: any },
    ) {}
  }
  class LanguageModelDataPart {
    constructor(
      public data: Uint8Array,
      public mimeType: string,
    ) {}
  }
  class LanguageModelToolCallPart {
    constructor(
      public callId: string,
      public name: string,
      public input: object,
    ) {}
  }
  class LanguageModelToolResultPart {
    constructor(
      public callId: string,
      public content: unknown[],
    ) {}
  }
  return {
    workspace: {
      workspaceFolders: undefined,
      getConfiguration: () => ({ get: () => approvalMode }),
    },
    LanguageModelChatMessageRole: { User: 1, Assistant: 2, System: 3 },
    get LanguageModelThinkingPart() {
      return thinkingAvailable ? LanguageModelThinkingPart : undefined;
    },
    LanguageModelTextPart,
    LanguageModelDataPart,
    LanguageModelToolCallPart,
    LanguageModelToolResultPart,
    /** Test hook: configure the reported manox.approvalMode. */
    setApprovalModeConfig: (mode: string | undefined) => {
      approvalMode = mode;
    },
    /** Test hook: simulate a host that does not inject the thinking part. */
    setThinkingPartAvailable: (available: boolean) => {
      thinkingAvailable = available;
    },
  };
});

import * as vscode from 'vscode';
import type { ActorEvent } from './protocol';
import { ManoxModelProvider, partToString, reportThinking, serializeTranscript } from './modelProvider';
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

  commands(): Record<string, unknown>[] {
    return this.sent.map((s) => JSON.parse(s) as Record<string, unknown>);
  }
}

const create = () => {
  const transport = new FakeTransport();
  const manager = new (SessionManager as unknown as new (t: Transport) => SessionManager)(
    transport,
  );
  return { transport, manager };
};

const textPart = (value: string) => new vscode.LanguageModelTextPart(value);
const msg = (role: number, content: unknown[], name?: string) =>
  ({ role, content, name: name ?? undefined }) as unknown as vscode.LanguageModelChatRequestMessage;
const userMsg = (text: string) =>
  msg(vscode.LanguageModelChatMessageRole.User, [textPart(text)]);
const assistantMsg = (content: unknown[]) =>
  msg(vscode.LanguageModelChatMessageRole.Assistant, content);

const modelInfo = (id: string): vscode.LanguageModelChatInformation => ({
  id,
  name: 'M',
  family: 'f',
  version: '1.0.0',
  maxInputTokens: 200_000,
  maxOutputTokens: 32_000,
  capabilities: { toolCalling: true, imageInput: true },
});

const progress = {
  report: vi.fn(),
} as unknown as vscode.Progress<vscode.LanguageModelResponsePart> & {
  report: ReturnType<typeof vi.fn>;
};

const emptyOptions = {} as vscode.ProvideLanguageModelChatResponseOptions;

/** Cancellation token whose flag flips when `cancel()` runs the callbacks. */
const fakeToken = () => {
  let cancelled = false;
  const cancelFns: Array<(e?: unknown) => void> = [];
  return {
    get isCancellationRequested() {
      return cancelled;
    },
    onCancellationRequested: vi.fn((f: (e?: unknown) => void) => {
      cancelFns.push(f);
      return { dispose: () => {} };
    }),
    cancel: () => {
      cancelled = true;
      for (const f of cancelFns) f();
    },
  } as unknown as vscode.CancellationToken & { cancel(): void };
};

/** Drain the microtask queue so actor responses propagate. */
const flush = async (times = 5) => {
  for (let i = 0; i < times; i++) await Promise.resolve();
};

/**
 * Start a response turn and drive it through init + create_session,
 * returning the session id.
 */
async function openTurn(
  transport: FakeTransport,
  run: Promise<void>,
): Promise<{ sessionId: string }> {
  await flush();
  transport.emit({ type: 'ready' });
  await flush();
  const createCmd = transport.lastCommand();
  expect(createCmd.cmd).toBe('create_session');
  const sessionId = createCmd.sessionId as string;
  transport.emit({ type: 'session_created', sessionId });
  await flush();
  return { sessionId };
}

/** Run one complete turn (fresh session per distinct first message). */
async function runTurn(
  provider: ManoxModelProvider,
  transport: FakeTransport,
  text: string,
): Promise<string> {
  const pending = provider.provideLanguageModelChatResponse(
    modelInfo('anthropic/x'),
    [userMsg(text)],
    emptyOptions,
    progress,
    fakeToken(),
  );
  await flush();
  if (transport.lastCommand().cmd === 'init') {
    transport.emit({ type: 'ready' });
    await flush();
  }
  const createCmd = transport.lastCommand();
  expect(createCmd.cmd).toBe('create_session');
  const sessionId = createCmd.sessionId as string;
  transport.emit({ type: 'session_created', sessionId });
  await flush();
  finishTurn(transport, sessionId);
  await pending;
  return sessionId;
}

const finishTurn = (transport: FakeTransport, sessionId: string) =>
  transport.emit({ type: 'turn_finished', sessionId, cancelled: false, failed: false });

const lastSubmit = (transport: FakeTransport) =>
  transport.commands().reverse().find((c) => c.cmd === 'submit') as Record<string, unknown>;

const setApprovalModeConfig = (mode: string | undefined) =>
  (vscode as unknown as { setApprovalModeConfig(m: string | undefined): void }).setApprovalModeConfig(mode);
const setThinkingPartAvailable = (available: boolean) =>
  (vscode as unknown as { setThinkingPartAvailable(a: boolean): void }).setThinkingPartAvailable(available);

afterEach(() => {
  vi.useRealTimers();
  progress.report.mockClear();
  setApprovalModeConfig(undefined);
  setThinkingPartAvailable(true);
});

describe('provideLanguageModelChatInformation', () => {
  it('maps actor models to user-selectable chat model information', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const pending = provider.provideLanguageModelChatInformation(
      { silent: false },
      fakeToken(),
    );
    await flush();
    expect(transport.lastCommand()).toEqual(expect.objectContaining({ cmd: 'init' }));
    transport.emit({ type: 'ready' });
    await flush();
    expect(transport.lastCommand()).toEqual({ cmd: 'list_models' });
    transport.emit({
      type: 'models',
      models: [{ id: 'anthropic/x', name: 'X', provider: 'anthropic' }],
    });
    await expect(pending).resolves.toEqual([
      expect.objectContaining({
        id: 'anthropic/x',
        name: 'X',
        family: 'anthropic',
        version: '1.0.0',
        maxInputTokens: 200_000,
        maxOutputTokens: 32_000,
        capabilities: { toolCalling: true, imageInput: true },
        isUserSelectable: true,
      }),
    ]);
  });

  it('stays silent for silent resolution', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    await expect(
      provider.provideLanguageModelChatInformation({ silent: true }, fakeToken()),
    ).resolves.toEqual([]);
    expect(transport.sent).toHaveLength(0);
  });

  it('returns no models when the actor never responds', async () => {
    vi.useFakeTimers();
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const pending = provider.provideLanguageModelChatInformation(
      { silent: false },
      fakeToken(),
    );
    await flush();
    transport.emit({ type: 'ready' });
    await vi.advanceTimersByTimeAsync(5_001);
    await expect(pending).resolves.toEqual([]);
  });
});

describe('provideLanguageModelChatResponse', () => {
  it('drives a fresh session and streams text and thinking parts', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const pending = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [userMsg('hello')],
      emptyOptions,
      progress,
      fakeToken(),
    );
    const { sessionId } = await openTurn(transport, pending);

    const commands = transport.commands();
    expect(commands.map((c) => c.cmd)).toEqual([
      'init',
      'create_session',
      'set_approval_mode',
      'set_model',
      'set_approval_mode',
      'submit',
    ]);
    // The default config (danger) is asserted at creation and re-asserted
    // with the per-turn model switch.
    expect(commands.filter((c) => c.cmd === 'set_approval_mode')).toEqual([
      { cmd: 'set_approval_mode', sessionId, mode: 'danger' },
      { cmd: 'set_approval_mode', sessionId, mode: 'danger' },
    ]);
    expect(commands.find((c) => c.cmd === 'set_model')).toEqual({
      cmd: 'set_model',
      sessionId,
      id: 'anthropic/x',
    });
    const submit = commands.find((c) => c.cmd === 'submit');
    expect(submit).toMatchObject({ cmd: 'submit', sessionId, text: 'hello' });
    expect(submit).not.toHaveProperty('images');

    transport.emit({ type: 'agent_text', sessionId, text: 'Hi there' });
    expect(progress.report.mock.calls[0][0]).toBeInstanceOf(vscode.LanguageModelTextPart);
    expect(progress.report.mock.calls[0][0].value).toBe('Hi there');

    transport.emit({ type: 'agent_thinking', sessionId, text: 'reasoning' });
    expect(progress.report.mock.calls[1][0]).toBeInstanceOf(
      vscode.LanguageModelThinkingPart,
    );
    expect(progress.report.mock.calls[1][0].value).toBe('reasoning');

    finishTurn(transport, sessionId);
    await expect(pending).resolves.toBeUndefined();
  });

  it('reuses the cached session and re-asserts model and danger on follow-ups', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const first = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [userMsg('first question')],
      emptyOptions,
      progress,
      fakeToken(),
    );
    const { sessionId } = await openTurn(transport, first);
    finishTurn(transport, sessionId);
    await first;
    const firstCount = transport.commands().length;

    const second = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [
        userMsg('first question'),
        assistantMsg([textPart('answer')]),
        userMsg('follow-up'),
      ],
      emptyOptions,
      progress,
      fakeToken(),
    );
    await flush();
    // Same conversation key: no new session, delta only, and the per-turn
    // model/approval re-assertion still fires.
    expect(transport.commands().filter((c) => c.cmd === 'create_session')).toHaveLength(1);
    const secondPhase = transport.commands().slice(firstCount);
    expect(secondPhase.map((c) => c.cmd)).toEqual([
      'set_model',
      'set_approval_mode',
      'submit',
    ]);
    expect(secondPhase[1]).toEqual({ cmd: 'set_approval_mode', sessionId, mode: 'danger' });
    const submit = lastSubmit(transport);
    expect(submit).toMatchObject({ cmd: 'submit', sessionId, text: 'follow-up' });
    finishTurn(transport, sessionId);
    await second;
  });

  it('seeds a fresh session with the prior transcript', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const prior = [assistantMsg([textPart('prior answer')])];
    const messages = [...prior, userMsg('question')];
    const pending = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      messages,
      emptyOptions,
      progress,
      fakeToken(),
    );
    const { sessionId } = await openTurn(transport, pending);
    const submit = lastSubmit(transport);
    expect(submit.text).toBe(serializeTranscript(prior) + '\n\n---\n\nquestion');
    finishTurn(transport, sessionId);
    await pending;
  });

  it('sends cancel_turn on cancellation and resolves on the cancelled finish', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const token = fakeToken();
    const pending = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [userMsg('hi')],
      emptyOptions,
      progress,
      token,
    );
    const { sessionId } = await openTurn(transport, pending);
    token.cancel();
    expect(transport.lastCommand()).toEqual({ cmd: 'cancel_turn', sessionId });
    transport.emit({ type: 'turn_finished', sessionId, cancelled: true, failed: false });
    await expect(pending).resolves.toBeUndefined();
  });

  it('rejects with the actor error message', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const pending = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [userMsg('hi')],
      emptyOptions,
      progress,
      fakeToken(),
    );
    const { sessionId } = await openTurn(transport, pending);
    transport.emit({ type: 'error', sessionId, message: 'boom' });
    await expect(pending).rejects.toThrow('boom');
  });

  it('rejects when the turn reports failure without an error event', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const pending = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [userMsg('hi')],
      emptyOptions,
      progress,
      fakeToken(),
    );
    const { sessionId } = await openTurn(transport, pending);
    transport.emit({ type: 'turn_finished', sessionId, cancelled: false, failed: true });
    await expect(pending).rejects.toThrow('agent turn failed');
  });

  it('rejects a request that collides with an in-flight session', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const first = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [userMsg('hello')],
      emptyOptions,
      progress,
      fakeToken(),
    );
    const { sessionId } = await openTurn(transport, first);
    const second = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [userMsg('hello')],
      emptyOptions,
      progress,
      fakeToken(),
    );
    await expect(second).rejects.toThrow(/busy/);
    finishTurn(transport, sessionId);
    await first;
  });

  it('rejects an empty delta and reclaims the fresh session', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const messages = [
      assistantMsg([textPart('prior')]),
      msg(vscode.LanguageModelChatMessageRole.User, []),
    ];
    const pending = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      messages,
      emptyOptions,
      progress,
      fakeToken(),
    );
    const { sessionId } = await openTurn(transport, pending);
    await expect(pending).rejects.toThrow(/empty agent request/);
    const disposed = transport.commands().filter((c) => c.cmd === 'dispose_session');
    expect(disposed.map((c) => c.sessionId)).toContain(sessionId);
  });

  it('sends base64 images from the last user message', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const first = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [userMsg('look')],
      emptyOptions,
      progress,
      fakeToken(),
    );
    const { sessionId } = await openTurn(transport, first);
    finishTurn(transport, sessionId);
    await first;

    const data = new Uint8Array([137, 80, 78, 71]);
    const second = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [
        userMsg('look'),
        assistantMsg([textPart('saw it')]),
        msg(vscode.LanguageModelChatMessageRole.User, [
          textPart('see '),
          new vscode.LanguageModelDataPart(data, 'image/png'),
        ]),
      ],
      emptyOptions,
      progress,
      fakeToken(),
    );
    await flush();
    const submit = lastSubmit(transport);
    expect(submit.text).toBe('see ');
    expect(submit.images).toEqual([
      { data: Buffer.from(data).toString('base64'), mimeType: 'image/png' },
    ]);
    finishTurn(transport, sessionId);
    await second;
  });

  it('aborts during actor init before any session is created', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const token = fakeToken();
    const pending = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [userMsg('hi')],
      emptyOptions,
      progress,
      token,
    );
    await flush();
    expect(transport.lastCommand()).toEqual(expect.objectContaining({ cmd: 'init' }));
    token.cancel();
    transport.emit({ type: 'ready' });
    await flush();
    await expect(pending).resolves.toBeUndefined();
    expect(transport.commands().filter((c) => c.cmd === 'create_session')).toHaveLength(0);
  });

  it('aborts during session creation and reclaims the session', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const token = fakeToken();
    const pending = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [userMsg('hi')],
      emptyOptions,
      progress,
      token,
    );
    await flush();
    transport.emit({ type: 'ready' });
    await flush();
    const createCmd = transport.lastCommand();
    expect(createCmd.cmd).toBe('create_session');
    const sessionId = createCmd.sessionId as string;
    token.cancel();
    transport.emit({ type: 'session_created', sessionId });
    await flush();
    await expect(pending).resolves.toBeUndefined();
    expect(transport.commands().filter((c) => c.cmd === 'submit')).toHaveLength(0);
    expect(
      transport.commands().filter((c) => c.cmd === 'dispose_session').map((c) => c.sessionId),
    ).toContain(sessionId);
  });

  it('overrides an autopilot config with danger on every turn', async () => {
    setApprovalModeConfig('autopilot');
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const first = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [userMsg('hi')],
      emptyOptions,
      progress,
      fakeToken(),
    );
    const { sessionId } = await openTurn(transport, first);
    // createSession inherits autopilot from the config; the provider's
    // re-assertion converges the session to danger.
    const approvals = transport.commands().filter((c) => c.cmd === 'set_approval_mode');
    expect(approvals.map((c) => c.mode)).toEqual(['autopilot', 'danger']);
    finishTurn(transport, sessionId);
    await first;

    const second = provider.provideLanguageModelChatResponse(
      modelInfo('anthropic/x'),
      [userMsg('hi'), assistantMsg([textPart('a')]), userMsg('again')],
      emptyOptions,
      progress,
      fakeToken(),
    );
    await flush();
    const lastApproval = transport
      .commands()
      .filter((c) => c.cmd === 'set_approval_mode')
      .at(-1);
    expect(lastApproval).toEqual({ cmd: 'set_approval_mode', sessionId, mode: 'danger' });
    finishTurn(transport, sessionId);
    await second;
  });
});

describe('eviction', () => {
  it('evicts idle sessions past the TTL', async () => {
    vi.useFakeTimers();
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const idle = await runTurn(provider, transport, 'a');
    await vi.advanceTimersByTimeAsync(30 * 60_000 + 1);
    await runTurn(provider, transport, 'b');
    const disposed = transport
      .commands()
      .filter((c) => c.cmd === 'dispose_session')
      .map((c) => c.sessionId);
    expect(disposed).toContain(idle);
  });

  it('evicts least-recently-used sessions past the cap', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const ids: string[] = [];
    for (let i = 0; i < 17; i++) {
      ids.push(await runTurn(provider, transport, `seed ${i}`));
    }
    const disposed = transport
      .commands()
      .filter((c) => c.cmd === 'dispose_session')
      .map((c) => c.sessionId);
    expect(disposed).toContain(ids[0]);
    expect(disposed).not.toContain(ids[ids.length - 1]);
  });
});

describe('thinking fallback', () => {
  it('streams a text part when the host lacks the thinking part', () => {
    setThinkingPartAvailable(false);
    const p = { report: vi.fn() } as unknown as vscode.Progress<vscode.LanguageModelResponsePart>;
    reportThinking(p, 'reasoning');
    expect(p.report).toHaveBeenCalledTimes(1);
    expect(p.report).toHaveBeenCalledWith(expect.any(vscode.LanguageModelTextPart));
  });
});

describe('part serialization', () => {
  it('projects text, tool call, tool result, image, and unknown parts', () => {
    expect(partToString(new vscode.LanguageModelTextPart('hi'))).toBe('hi');
    expect(
      partToString(new vscode.LanguageModelToolCallPart('c1', 'read_file', { path: '/x' })),
    ).toBe('Tool call [read_file](c1): {"path":"/x"}');
    expect(
      partToString(new vscode.LanguageModelToolResultPart('c1', [textPart('output')])),
    ).toBe('Tool result (c1): output');
    expect(
      partToString(new vscode.LanguageModelDataPart(new Uint8Array([1, 2]), 'image/png')),
    ).toBe('<image image/png>');
    expect(partToString({ weird: true })).toBe('{"weird":true}');
    const circular: Record<string, unknown> = {};
    circular.self = circular;
    expect(partToString(circular)).toBe('[unserializable part]');
  });

  it('maps roles and joins messages for the transcript projection', () => {
    const messages = [
      userMsg('q'),
      assistantMsg([textPart('a')]),
      msg(3, [textPart('sys')]),
    ];
    expect(serializeTranscript(messages)).toBe(
      'Role: User: q\n\nRole: Assistant: a\n\nRole: System: sys',
    );
  });
});
