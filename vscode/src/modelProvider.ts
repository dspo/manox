// LanguageModelChatProvider that exposes manox as a bare chat model in the
// native chat. Each request is one stateless completion over the actor's
// provider layer (`model_chat`): wire messages and tool definitions go to the
// pi provider runtime, text/thinking deltas stream back as response parts,
// and tool calls the model emits are relayed as `LanguageModelToolCallPart`s
// for VS Code to execute — their results arrive on the next request as
// `tool_result` blocks. No agent session, approval, or manox tooling is
// involved.

import { randomUUID } from 'node:crypto';
import * as vscode from 'vscode';
import type { ActorEvent, ModelChatBlock, ModelChatMessage, ModelChatTool, ModelInfo } from './protocol';
import { SessionManager, resolveWorkspaceCwd } from './sessionManager';

const DEFAULT_MAX_INPUT_TOKENS = 200_000;
const DEFAULT_MAX_OUTPUT_TOKENS = 32_000;

/** Stable text projection of one message part (tool results, unknown parts). */
export function partToText(part: unknown): string {
  if (part instanceof vscode.LanguageModelTextPart) return part.value;
  if (part instanceof vscode.LanguageModelToolResultPart) {
    return part.content.map(partToText).join('');
  }
  try {
    const json = JSON.stringify(part);
    return json === undefined ? '[unserializable part]' : json;
  } catch {
    return '[unserializable part]';
  }
}

/** One VS Code message part → a wire block for `model_chat`. */
function partToWire(part: unknown): ModelChatBlock {
  if (part instanceof vscode.LanguageModelTextPart) {
    return { type: 'text', text: part.value };
  }
  if (part instanceof vscode.LanguageModelThinkingPart) {
    const value = Array.isArray(part.value) ? part.value.join('') : part.value;
    return { type: 'thinking', text: value };
  }
  if (part instanceof vscode.LanguageModelToolCallPart) {
    return { type: 'tool_call', id: part.callId, name: part.name, input: part.input };
  }
  if (part instanceof vscode.LanguageModelToolResultPart) {
    // isError is not yet in the stable typings; read it defensively so a
    // failed tool execution is not relayed to the model as a success.
    const isError = (part as unknown as { isError?: boolean }).isError;
    return {
      type: 'tool_result',
      id: part.callId,
      content: partToText(part),
      ...(isError ? { isError } : {}),
    };
  }
  if (part instanceof vscode.LanguageModelDataPart) {
    if (part.mimeType.startsWith('image/')) {
      return {
        type: 'image',
        data: Buffer.from(part.data).toString('base64'),
        mimeType: part.mimeType,
      };
    }
  }
  return { type: 'text', text: partToText(part) };
}

/** Full conversation → wire messages for `model_chat`. */
export function toWireMessages(
  messages: readonly vscode.LanguageModelChatRequestMessage[],
): ModelChatMessage[] {
  // The stable role enum exposes User/Assistant only; a hypothetical third
  // role value (the host's System) maps to the wire's system role.
  const roles: Record<number, ModelChatMessage['role']> = {
    [vscode.LanguageModelChatMessageRole.User]: 'user',
    [vscode.LanguageModelChatMessageRole.Assistant]: 'assistant',
    3: 'system',
  };
  return messages.map((m) => ({
    role: roles[m.role] ?? 'user',
    content: m.content.map(partToWire),
  }));
}

/** A manox group declared in the language models config file. */
export interface ManoxGroup {
  name: string;
  provider: string;
}

/** Candidate paths for VS Code's language models config file (user-data dir,
 * default profile) across platforms and builds. */
export function manoxConfigCandidates(): string[] {
  const appDir = (() => {
    switch (vscode.env.uriScheme) {
      case 'vscode-insiders':
        return 'Code - Insiders';
      case 'vscode-oss':
        return process.platform === 'darwin' ? 'Code - OSS' : 'code-oss';
      default:
        return 'Code';
    }
  })();
  const root =
    process.platform === 'darwin'
      ? `${process.env.HOME}/Library/Application Support`
      : process.platform === 'linux'
        ? `${process.env.HOME}/.config`
        : process.platform === 'win32'
          ? (process.env.APPDATA ?? '')
          : '';
  return root ? [`${root}/${appDir}/User/chatLanguageModels.json`] : [];
}

/** Parse the config file into manox's groups (vendor === 'manox'). */
export function parseManoxGroups(json: string): ManoxGroup[] {
  try {
    const groups: unknown = JSON.parse(json);
    if (!Array.isArray(groups)) return [];
    const out: ManoxGroup[] = [];
    for (const g of groups) {
      if (g && typeof g === 'object' && (g as { vendor?: unknown }).vendor === 'manox') {
        const { name, provider } = g as { name?: unknown; provider?: unknown };
        if (typeof name === 'string' && typeof provider === 'string') {
          out.push({ name, provider });
        }
      }
    }
    return out;
  } catch {
    return [];
  }
}

/** Read manox's groups from the config file; empty when the file is absent,
 * unreadable, or declares no manox groups. */
async function readManoxGroups(): Promise<ManoxGroup[]> {
  for (const path of manoxConfigCandidates()) {
    try {
      const bytes = await vscode.workspace.fs.readFile(vscode.Uri.file(path));
      return parseManoxGroups(bytes.toString());
    } catch {
      // try the next candidate
    }
  }
  return [];
}

/** One actor model → picker-visible model information. */
function toModelInfo(m: ModelInfo): vscode.LanguageModelChatInformation {
  return {
    id: m.id,
    name: m.name,
    family: m.provider,
    version: '1.0.0',
    // The actor reports the provider's real context window; a missing
    // value falls back to the placeholder.
    maxInputTokens: m.context_window || DEFAULT_MAX_INPUT_TOKENS,
    maxOutputTokens: m.max_tokens || DEFAULT_MAX_OUTPUT_TOKENS,
    capabilities: { toolCalling: true, imageInput: true },
    isUserSelectable: true,
  };
}

/** Derive a manox group per cx provider and persist it into the language
 * models config file, preserving every existing entry. The workbench watches
 * the file and re-resolves the vendor on change, which turns the flat
 * listing into named groups. Idempotent: the caller only invokes this when
 * the file declares no manox groups yet. */
async function createManoxGroups(models: ModelInfo[]): Promise<void> {
  const providers = Array.from(
    new Set(models.map((m) => m.provider_name ?? m.provider)),
  )
    .filter((p): p is string => typeof p === 'string' && p.length > 0)
    .sort();
  if (providers.length === 0) return;
  const configFile = manoxConfigCandidates()[0];
  if (!configFile) return;
  const uri = vscode.Uri.file(configFile);
  let allGroups: unknown[] = [];
  try {
    const bytes = await vscode.workspace.fs.readFile(uri);
    const parsed: unknown = JSON.parse(bytes.toString());
    if (Array.isArray(parsed)) allGroups = parsed;
  } catch {
    // absent or unparseable — start from an empty list
  }
  allGroups.push(...providers.map((provider) => ({ vendor: 'manox', name: `Manox-${provider}`, provider })));
  try {
    await vscode.workspace.fs.writeFile(
      uri,
      Buffer.from(JSON.stringify(allGroups, undefined, '\t'), 'utf8'),
    );
  } catch {
    // A failed write degrades to the flat listing for this session.
  }
}

export class ManoxModelProvider implements vscode.LanguageModelChatProvider {
  constructor(private readonly manager: SessionManager) {}

  async provideLanguageModelChatInformation(
    options: vscode.PrepareLanguageModelChatModelOptions,
    _token: vscode.CancellationToken,
  ): Promise<vscode.LanguageModelChatInformation[]> {
    // VS Code enumerates a vendor's models with silent=true (picker and the
    // Manage Models table), and does two shapes of call: an initial groupless
    // listing (configuration absent) plus one call per group declared in the
    // language models config file (configuration carries the group's own
    // properties). manox needs no user interaction to list models, so both
    // paths resolve them.
    if (options.configuration !== undefined) {
      // Per-group call: configuration.provider names the cx provider this
      // group exposes; serve only that provider's models so the group bucket
      // is not polluted by every other provider.
      const providerName = options.configuration.provider;
      if (typeof providerName !== 'string') return [];
      try {
        await this.manager.init(resolveWorkspaceCwd());
        const models = await this.manager.listModels();
        return models
          .filter((m) => (m.provider_name ?? m.provider) === providerName)
          .map(toModelInfo);
      } catch {
        return [];
      }
    }
    // Groupless call. When manox groups exist in the config file, the
    // per-group calls above serve the models and an ungrouped bucket would
    // duplicate them — return nothing. Without groups, derive one group per
    // cx provider, persist it (the workbench's file watch re-resolves the
    // vendor into named groups), and keep the flat listing for this call.
    const groups = await readManoxGroups();
    if (groups.length > 0) return [];
    try {
      await this.manager.init(resolveWorkspaceCwd());
      const models = await this.manager.listModels();
      await createManoxGroups(models);
      return models.map(toModelInfo);
    } catch {
      // A failing actor must not break the model picker; the vendor simply
      // does not show up until the actor responds.
      return [];
    }
  }

  async provideLanguageModelChatResponse(
    model: vscode.LanguageModelChatInformation,
    messages: readonly vscode.LanguageModelChatRequestMessage[],
    options: vscode.ProvideLanguageModelChatResponseOptions,
    progress: vscode.Progress<vscode.LanguageModelResponsePart>,
    token: vscode.CancellationToken,
  ): Promise<void> {
    await this.manager.init(resolveWorkspaceCwd());
    if (token.isCancellationRequested) return;

    const requestId = randomUUID();
    const tools: ModelChatTool[] = (options.tools ?? []).map((t) => ({
      name: t.name,
      description: t.description,
      inputSchema: (t.inputSchema ?? {}) as Record<string, unknown>,
    }));

    let settled = false;
    let resolveDone!: () => void;
    let rejectDone!: (error: Error) => void;
    const done = new Promise<void>((resolve, reject) => {
      resolveDone = resolve;
      rejectDone = reject;
    });
    const off = this.manager.onGlobalEvent((ev: ActorEvent) => {
      if (!('requestId' in ev) || ev.requestId !== requestId) return;
      switch (ev.type) {
        case 'model_text':
          progress.report(new vscode.LanguageModelTextPart(ev.text));
          break;
        case 'model_thinking':
          // The thinking part is a proposed type kept out of the stable
          // `LanguageModelResponsePart` union; the report target accepts it
          // on hosts that declare the proposal.
          progress.report(
            new vscode.LanguageModelThinkingPart(
              ev.text,
            ) as unknown as vscode.LanguageModelResponsePart,
          );
          break;
        case 'model_tool_call':
          progress.report(
            new vscode.LanguageModelToolCallPart(ev.id, ev.name, ev.input as object),
          );
          break;
        case 'model_chat_done':
          if (settled) return;
          settled = true;
          off();
          cancelSub.dispose();
          // A tool-use stop is a normal end: VS Code executes the relayed
          // tools and calls back with their results on the next request.
          if (ev.error !== null) rejectDone(new Error(ev.error));
          else resolveDone();
          break;
      }
    });
    const cancelSub = token.onCancellationRequested(() => {
      this.manager.send({ cmd: 'cancel_model_chat', requestId });
    });

    try {
      this.manager.send({
        cmd: 'model_chat',
        requestId,
        model: model.id,
        messages: toWireMessages(messages),
        tools,
      });
      await done;
    } finally {
      if (!settled) {
        settled = true;
        off();
        cancelSub.dispose();
      }
    }
  }

  async provideTokenCount(
    _model: vscode.LanguageModelChatInformation,
    text: string | vscode.LanguageModelChatRequestMessage,
    _token: vscode.CancellationToken,
  ): Promise<number> {
    // Heuristic (chars/4): the actor exposes no standalone tokenizer; the
    // approximation is only consumed by UI affordances.
    const value = typeof text === 'string' ? text : text.content.map(partToText).join('');
    return Math.ceil(value.length / 4);
  }
}
