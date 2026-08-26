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
import type { ActorEvent, ModelChatBlock, ModelChatMessage, ModelChatTool, ModelInfo } from '../dist/protocol';
import { SessionManager, resolveWorkspaceCwd } from './sessionManager';

const DEFAULT_MAX_INPUT_TOKENS = 200_000;
const DEFAULT_MAX_OUTPUT_TOKENS = 32_000;
// Groupless (silent) enumerations run on every picker open / default-model
// lookup; re-booting the actor each time has real cost. Reuse the last sync
// within this window — cx providers added afterwards are picked up on the next
// window, an acceptable delay for the silent path.
const SYNC_TTL_MS = 60_000;

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

/** Path of VS Code's language models config file (local user-data dir, default
 * profile) for the current platform and build; undefined when the host has no
 * resolvable user-data root. */
export function manoxConfigFile(): string | undefined {
  // Editor forks keep their own app dir; map the common uriSchemes so the
  // write lands where the running workbench watches. Unknown forks fall back
  // to 'Code' (they'd otherwise resolve to a non-existent dir).
  const appDir = (() => {
    switch (vscode.env.uriScheme) {
      case 'vscode-insiders':
        return 'Code - Insiders';
      case 'vscode-oss':
        return process.platform === 'darwin' ? 'Code - OSS' : 'code-oss';
      case 'cursor':
        return 'Cursor';
      case 'windsurf':
        return 'Windsurf';
      case 'vscodium':
        return 'VSCodium';
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
  return root ? `${root}/${appDir}/User/chatLanguageModels.json` : undefined;
}

/** The full parsed group array from the config file; [] when absent. */
async function readConfigGroups(): Promise<unknown[]> {
  const path = manoxConfigFile();
  if (!path) return [];
  try {
    const bytes = await vscode.workspace.fs.readFile(vscode.Uri.file(path));
    const parsed: unknown = JSON.parse(bytes.toString());
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

/** Filter the parsed config array down to manox's groups. */
export function manoxGroupsOf(all: unknown[]): ManoxGroup[] {
  const out: ManoxGroup[] = [];
  for (const g of all) {
    if (g && typeof g === 'object' && (g as { vendor?: unknown }).vendor === 'manox') {
      const { name, provider } = g as { name?: unknown; provider?: unknown };
      if (typeof name === 'string' && typeof provider === 'string') {
        out.push({ name, provider });
      }
    }
  }
  return out;
}

/** Parse the config file into manox's groups (vendor === 'manox'). */
export function parseManoxGroups(json: string): ManoxGroup[] {
  try {
    const parsed: unknown = JSON.parse(json);
    return manoxGroupsOf(Array.isArray(parsed) ? parsed : []);
  } catch {
    return [];
  }
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

/** VS Code registers each group bucket's models by id; one provider's wire
 * variants (anthropic/responses/completions endpoints of the same model)
 * share an id, so a bucket may hold several. Keep the first registration
 * (sorted by registration name) so the native chat never sees an "already
 * registered" collision. */
function dedupById(models: ModelInfo[]): ModelInfo[] {
  const seen = new Set<string>();
  return models.filter((m) => {
    if (seen.has(m.id)) return false;
    seen.add(m.id);
    return true;
  });
}

/** Keep the config file's manox groups in sync with the current cx providers:
 * append one `Manox-<provider>` group for every provider not yet covered,
 * preserving existing entries. Idempotent — providers already covered are
 * never duplicated, so later enumerations pick up providers added to
 * cx.providers.config.yaml after the first run. Remote degradation target:
 * the picker shows the flat listing; named groups remain a local-session
 * feature. Never writes on a remote
 * host: chatLanguageModels.json lives on the local side where the workbench
 * watches it, and `workspace.fs` would target the remote FS there. */
async function syncManoxGroups(
  manager: SessionManager,
): Promise<{ models: ModelInfo[]; grouped: boolean }> {
  let models: ModelInfo[] = [];
  try {
    await manager.init(resolveWorkspaceCwd());
    models = await manager.listModels();
  } catch {
    // A failing actor must not break the model picker; the vendor simply
    // does not show up until the actor responds.
    return { models: [], grouped: false };
  }
  const all = await readConfigGroups();
  const existing = manoxGroupsOf(all);
  const providers = Array.from(new Set(models.map((m) => m.provider_name ?? m.provider)))
    .filter((p): p is string => typeof p === 'string' && p.length > 0)
    .sort();
  const covered = new Set(existing.map((g) => g.provider));
  const missing = providers.filter((p) => !covered.has(p));
  if (missing.length === 0) return { models, grouped: existing.length > 0 };
  // Remote host: chatLanguageModels.json is managed locally while
  // workspace.fs targets the remote FS — skip the write and degrade to the
  // flat listing instead of silently hiding the models.
  if (vscode.env.remoteName) return { models, grouped: existing.length > 0 };
  const path = manoxConfigFile();
  if (!path) return { models, grouped: existing.length > 0 };
  const uri = vscode.Uri.file(path);
  all.push(...missing.map((provider) => ({ vendor: 'manox', name: `Manox-${provider}`, provider })));
  try {
    await vscode.workspace.fs.writeFile(
      uri,
      Buffer.from(JSON.stringify(all, undefined, '\t'), 'utf8'),
    );
  } catch {
    // A failed write degrades to the flat listing for this session.
  }
  // Groups now exist (we just appended the missing ones); named groups serve.
  return { models, grouped: true };
}

export class ManoxModelProvider implements vscode.LanguageModelChatProvider {
  private syncCache: { at: number; models: ModelInfo[]; grouped: boolean } | undefined;

  constructor(private readonly manager: SessionManager) {}

  /** Groupless resolution with a short-lived in-memory cache so repeated
   * silent enumerations do not re-init the actor (see SYNC_TTL_MS). */
  private async resolveGroupless(): Promise<{ models: ModelInfo[]; grouped: boolean }> {
    const now = Date.now();
    if (this.syncCache && now - this.syncCache.at < SYNC_TTL_MS) return this.syncCache;
    const result = await syncManoxGroups(this.manager);
    this.syncCache = { at: now, ...result };
    return result;
  }

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
        return dedupById(
          models.filter((m) => (m.provider_name ?? m.provider) === providerName),
        ).map(toModelInfo);
      } catch {
        return [];
      }
    }
    // Groupless call. Sync the config file's manox groups with the current
    // cx providers (appending any not yet covered). When named groups now
    // exist they serve the models via the per-group calls above, so an
    // ungrouped bucket would duplicate them — return nothing. Only when no
    // cx provider exists at all do we fall back to the flat listing.
    const { models, grouped } = await this.resolveGroupless();
    if (grouped) return [];
    return dedupById(models).map(toModelInfo);
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
