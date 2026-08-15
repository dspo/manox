// ManoxModelProvider behaviour against an in-memory transport: model
// information mapping, the stateless model_chat payload (message/tool wire
// conversion), delta streaming, tool-call relay, cancellation, and failure
// settlement.

import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('vscode', () => {
  class LanguageModelTextPart {
    constructor(public value: string) {}
  }
  class LanguageModelThinkingPart {
    constructor(
      public value: string | string[],
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
      getConfiguration: () => ({ get: () => undefined }),
    },
    LanguageModelChatMessageRole: { User: 1, Assistant: 2, System: 3 },
    LanguageModelTextPart,
    LanguageModelThinkingPart,
    LanguageModelDataPart,
    LanguageModelToolCallPart,
    LanguageModelToolResultPart,
  };
});

import * as vscode from 'vscode';
import type { ActorEvent } from './protocol';
import { ManoxModelProvider, partToText, toWireMessages } from './modelProvider';
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
const msg = (role: number, content: unknown[]) =>
  ({ role, content, name: undefined }) as unknown as vscode.LanguageModelChatRequestMessage;
const userMsg = (text: string) =>
  msg(vscode.LanguageModelChatMessageRole.User, [textPart(text)]);

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

/** Start a response, drive init to completion, and resolve the request id. */
async function openChat(
  provider: ManoxModelProvider,
  transport: FakeTransport,
  messages: readonly vscode.LanguageModelChatRequestMessage[],
  options: vscode.ProvideLanguageModelChatResponseOptions = emptyOptions,
  token = fakeToken(),
): Promise<{ pending: Promise<void>; requestId: string; modelChat: Record<string, unknown> }> {
  const pending = provider.provideLanguageModelChatResponse(
    modelInfo('anthropic/x'),
    messages,
    options,
    progress,
    token,
  );
  await flush();
  expect(transport.lastCommand()).toEqual(expect.objectContaining({ cmd: 'init' }));
  transport.emit({ type: 'ready' });
  await flush();
  const modelChat = transport.commands().find((c) => c.cmd === 'model_chat')!;
  return { pending, requestId: modelChat.requestId as string, modelChat };
}

afterEach(() => {
  progress.report.mockClear();
});

describe('provideLanguageModelChatInformation', () => {
  it('maps actor models to user-selectable chat model information', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const pending = provider.provideLanguageModelChatInformation({ silent: false }, fakeToken());
    await flush();
    transport.emit({ type: 'ready' });
    await flush();
    expect(transport.lastCommand()).toEqual({ cmd: 'list_models' });
    transport.emit({
      type: 'models',
      models: [
        {
          id: 'anthropic/x',
          name: 'X',
          provider: 'anthropic',
          api: 'anthropic',
          context_window: 131_072,
          max_tokens: 8_192,
        },
      ],
    });
    await expect(pending).resolves.toEqual([
      expect.objectContaining({
        id: 'anthropic/x',
        name: 'X',
        family: 'anthropic',
        version: '1.0.0',
        maxInputTokens: 131_072,
        maxOutputTokens: 8_192,
        capabilities: { toolCalling: true, imageInput: true },
        isUserSelectable: true,
      }),
    ]);
  });

  it('falls back to placeholder windows for a zero or absent budget', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const pending = provider.provideLanguageModelChatInformation({ silent: false }, fakeToken());
    await flush();
    transport.emit({ type: 'ready' });
    await flush();
    transport.emit({
      type: 'models',
      models: [
        {
          id: 'anthropic/x',
          name: 'X',
          provider: 'anthropic',
          api: 'anthropic',
          context_window: 0,
        },
      ],
    });
    await expect(pending).resolves.toEqual([
      expect.objectContaining({ maxInputTokens: 200_000, maxOutputTokens: 32_000 }),
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
    const pending = provider.provideLanguageModelChatInformation({ silent: false }, fakeToken());
    await flush();
    transport.emit({ type: 'ready' });
    await vi.advanceTimersByTimeAsync(5_001);
    await expect(pending).resolves.toEqual([]);
  });
});

describe('provideLanguageModelChatResponse', () => {
  it('sends a model_chat request and streams text, thinking, and tool-call parts', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const { pending, requestId, modelChat } = await openChat(provider, transport, [
      userMsg('list files'),
    ]);

    expect(modelChat).toMatchObject({
      cmd: 'model_chat',
      model: 'anthropic/x',
      messages: [{ role: 'user', content: [{ type: 'text', text: 'list files' }] }],
      tools: [],
    });

    transport.emit({ type: 'model_text', requestId, text: 'Here are ' });
    expect(progress.report.mock.calls[0][0]).toBeInstanceOf(vscode.LanguageModelTextPart);
    expect(progress.report.mock.calls[0][0].value).toBe('Here are ');

    transport.emit({ type: 'model_thinking', requestId, text: 'reasoning' });
    expect(progress.report.mock.calls[1][0]).toBeInstanceOf(
      vscode.LanguageModelThinkingPart,
    );
    expect(progress.report.mock.calls[1][0].value).toBe('reasoning');

    transport.emit({
      type: 'model_tool_call',
      requestId,
      id: 'c1',
      name: 'listDir',
      input: { path: '.' },
    });
    const toolCall = progress.report.mock.calls[2][0];
    expect(toolCall).toBeInstanceOf(vscode.LanguageModelToolCallPart);
    expect(toolCall.callId).toBe('c1');
    expect(toolCall.name).toBe('listDir');
    expect(toolCall.input).toEqual({ path: '.' });

    // A tool-use stop settles the request normally: VS Code executes the
    // relayed tools and calls back on the next request.
    transport.emit({ type: 'model_chat_done', requestId, stop: 'toolUse', error: null });
    await expect(pending).resolves.toBeUndefined();
  });

  it('relays tool definitions from the native chat', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const options = {
      tools: [{ name: 'listDir', description: 'List a directory', inputSchema: { type: 'object' } }],
    } as unknown as vscode.ProvideLanguageModelChatResponseOptions;
    const { pending, requestId, modelChat } = await openChat(
      provider,
      transport,
      [userMsg('hi')],
      options,
    );
    expect(modelChat.tools).toEqual([
      { name: 'listDir', description: 'List a directory', inputSchema: { type: 'object' } },
    ]);
    transport.emit({ type: 'model_chat_done', requestId, stop: 'stop', error: null });
    await pending;
  });

  it('rejects with the actor error message', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const { pending, requestId } = await openChat(provider, transport, [userMsg('hi')]);
    transport.emit({ type: 'model_chat_done', requestId, stop: null, error: 'boom' });
    await expect(pending).rejects.toThrow('boom');
  });

  it('sends cancel_model_chat on cancellation and resolves on the settlement', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const token = fakeToken();
    const { pending, requestId } = await openChat(
      provider,
      transport,
      [userMsg('hi')],
      emptyOptions,
      token,
    );
    token.cancel();
    expect(transport.lastCommand()).toEqual({ cmd: 'cancel_model_chat', requestId });
    transport.emit({ type: 'model_chat_done', requestId, stop: 'aborted', error: null });
    await expect(pending).resolves.toBeUndefined();
  });

  it('ignores events for other requests and unrelated global events', async () => {
    const { transport, manager } = create();
    const provider = new ManoxModelProvider(manager);
    const { pending, requestId } = await openChat(provider, transport, [userMsg('hi')]);
    transport.emit({ type: 'model_text', requestId: 'other', text: 'intruder' });
    transport.emit({ type: 'threads_updated', threads: [] });
    expect(progress.report).not.toHaveBeenCalled();
    transport.emit({ type: 'model_chat_done', requestId, stop: 'stop', error: null });
    await pending;
  });
});

describe('toWireMessages', () => {
  it('maps every part shape to its wire block', () => {
    const messages = [
      msg(3, [textPart('be terse')]),
      msg(vscode.LanguageModelChatMessageRole.Assistant, [
        new vscode.LanguageModelThinkingPart(['hmm', '...']),
        new vscode.LanguageModelToolCallPart('c1', 'read', { path: '/x' }),
      ]),
      msg(vscode.LanguageModelChatMessageRole.User, [
        textPart('see '),
        new vscode.LanguageModelToolResultPart('c1', [textPart('out'), textPart(' more')]),
        new vscode.LanguageModelDataPart(new Uint8Array([1, 2]), 'image/png'),
      ]),
    ];
    expect(toWireMessages(messages)).toEqual([
      { role: 'system', content: [{ type: 'text', text: 'be terse' }] },
      {
        role: 'assistant',
        content: [
          { type: 'thinking', text: 'hmm...' },
          { type: 'tool_call', id: 'c1', name: 'read', input: { path: '/x' } },
        ],
      },
      {
        role: 'user',
        content: [
          { type: 'text', text: 'see ' },
          { type: 'tool_result', id: 'c1', content: 'out more' },
          { type: 'image', data: Buffer.from([1, 2]).toString('base64'), mimeType: 'image/png' },
        ],
      },
    ]);
  });

  it('falls back to a text block for unknown and non-image data parts', () => {
    const messages = [
      msg(vscode.LanguageModelChatMessageRole.User, [
        new vscode.LanguageModelDataPart(new Uint8Array([1]), 'application/pdf'),
        { weird: true },
      ]),
    ];
    expect(toWireMessages(messages)).toEqual([
      {
        role: 'user',
        content: [
          { type: 'text', text: JSON.stringify(new vscode.LanguageModelDataPart(new Uint8Array([1]), 'application/pdf')) },
          { type: 'text', text: '{"weird":true}' },
        ],
      },
    ]);
  });
});

describe('partToText', () => {
  it('projects text, nested tool results, unknown parts, and circular values', () => {
    expect(partToText(textPart('hi'))).toBe('hi');
    expect(
      partToText(new vscode.LanguageModelToolResultPart('c1', [textPart('a'), textPart('b')])),
    ).toBe('ab');
    expect(partToText({ weird: true })).toBe('{"weird":true}');
    const circular: Record<string, unknown> = {};
    circular.self = circular;
    expect(partToText(circular)).toBe('[unserializable part]');
  });
});
