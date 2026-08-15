// LanguageModelChatProvider that exposes manox as a chat model in the
// native chat. Each request drives one agent turn on a persistent actor
// session; the cache keys sessions by the conversation's first user message
// so follow-up turns continue the same transcript. Tool calls stay inside
// the agent loop and never surface as response parts.

import { createHash } from 'node:crypto';
import * as vscode from 'vscode';
import type { ActorEvent, ImageAttachment } from './protocol';
import { SessionManager, resolveWorkspaceCwd } from './sessionManager';

const DEFAULT_MAX_INPUT_TOKENS = 200_000;
const DEFAULT_MAX_OUTPUT_TOKENS = 32_000;
const MAX_SESSIONS = 16;
const SESSION_IDLE_TTL_MS = 30 * 60_000;

interface SessionEntry {
  sessionId: string;
  lastUsed: number;
}

/** Role ordinal → label used in the transcript seed projection. */
const ROLE_NAMES: Record<number, string> = { 1: 'User', 2: 'Assistant', 3: 'System' };

/**
 * Conversation identity: a hash of the first user message's text parts.
 * Appended turns keep the key stable; editing that first message starts a
 * fresh conversation (and loses the cached session's memory).
 */
export function conversationKey(
  messages: readonly vscode.LanguageModelChatRequestMessage[],
): string {
  const firstUser = messages.find(
    (m) => m.role === vscode.LanguageModelChatMessageRole.User,
  );
  const source = firstUser
    ? firstUser.content
        .map((part) => {
          const text = (part as { value?: unknown }).value;
          return typeof text === 'string' ? text : '';
        })
        .join(' ')
    : '<empty>';
  return createHash('sha256').update(source).digest('hex').slice(0, 16);
}

/** Stable text projection of one message part. */
export function partToString(part: unknown): string {
  if (part instanceof vscode.LanguageModelTextPart) return part.value;
  if (part instanceof vscode.LanguageModelToolCallPart) {
    return `Tool call [${part.name}](${part.callId}): ${JSON.stringify(part.input)}`;
  }
  if (part instanceof vscode.LanguageModelToolResultPart) {
    return `Tool result (${part.callId}): ${part.content.map(partToString).join('')}`;
  }
  if (part instanceof vscode.LanguageModelDataPart) {
    if (part.mimeType.startsWith('image/')) return `<image ${part.mimeType}>`;
  }
  try {
    const json = JSON.stringify(part);
    return json === undefined ? '[unserializable part]' : json;
  } catch {
    return '[unserializable part]';
  }
}

/** Full transcript projection: every message as `Role: <parts>`. */
export function serializeTranscript(
  messages: readonly vscode.LanguageModelChatRequestMessage[],
): string {
  return messages
    .map((m) => {
      const role = ROLE_NAMES[m.role] ?? String(m.role);
      return `Role: ${role}: ${m.content.map(partToString).join('')}`;
    })
    .join('\n\n');
}

/**
 * Split the last user message into plain text (text and non-image parts) and
 * base64 images for the submit channel.
 */
export function extractDelta(
  messages: readonly vscode.LanguageModelChatRequestMessage[],
): { text: string; images: ImageAttachment[] } {
  const lastUser = [...messages].reverse().find(
    (m) => m.role === vscode.LanguageModelChatMessageRole.User,
  );
  if (!lastUser) return { text: '', images: [] };
  const text: string[] = [];
  const images: ImageAttachment[] = [];
  for (const part of lastUser.content) {
    if (part instanceof vscode.LanguageModelDataPart && part.mimeType.startsWith('image/')) {
      images.push({
        data: Buffer.from(part.data).toString('base64'),
        mimeType: part.mimeType,
      });
    } else {
      text.push(partToString(part));
    }
  }
  return { text: text.join(''), images };
}

export class ManoxModelProvider implements vscode.LanguageModelChatProvider {
  private readonly sessions = new Map<string, SessionEntry>();

  constructor(private readonly manager: SessionManager) {}

  async provideLanguageModelChatInformation(
    options: vscode.PrepareLanguageModelChatModelOptions,
    _token: vscode.CancellationToken,
  ): Promise<vscode.LanguageModelChatInformation[]> {
    if (options.silent) return [];
    try {
      await this.manager.init(resolveWorkspaceCwd());
      const models = await this.manager.listModels();
      return models.map((m) => ({
        id: m.id,
        name: m.name,
        family: m.provider,
        version: '1.0.0',
        maxInputTokens: DEFAULT_MAX_INPUT_TOKENS,
        maxOutputTokens: DEFAULT_MAX_OUTPUT_TOKENS,
        capabilities: { toolCalling: true },
        isUserSelectable: true,
      }));
    } catch {
      // A failing actor must not break the model picker; the vendor simply
      // does not show up until the actor responds.
      return [];
    }
  }

  async provideLanguageModelChatResponse(
    model: vscode.LanguageModelChatInformation,
    messages: readonly vscode.LanguageModelChatRequestMessage[],
    _options: vscode.ProvideLanguageModelChatResponseOptions,
    progress: vscode.Progress<vscode.LanguageModelResponsePart>,
    token: vscode.CancellationToken,
  ): Promise<void> {
    const cwd = resolveWorkspaceCwd();
    await this.manager.init(cwd);

    this.evictStale();
    const key = conversationKey(messages);
    const existing = this.sessions.get(key);
    const isNewSession = !existing;
    let sessionId: string;
    if (existing) {
      existing.lastUsed = Date.now();
      sessionId = existing.sessionId;
    } else {
      sessionId = await this.manager.createSession(cwd);
      // Native chat has no approval surface, so autopilot would stall a turn
      // on a reviewer round-trip; force danger on fresh sessions.
      this.manager.send({ cmd: 'set_approval_mode', sessionId, mode: 'danger' });
      this.sessions.set(key, { sessionId, lastUsed: Date.now() });
    }

    const { text: deltaText, images } = extractDelta(messages);
    let text = deltaText;
    if (isNewSession && messages.length > 1) {
      // Seed the fresh session with everything before the last message so
      // the agent knows the conversation that led here; later turns only
      // send their delta and continue on the session transcript.
      text = serializeTranscript(messages.slice(0, -1)) + '\n\n---\n\n' + deltaText;
    }

    let settled = false;
    let resolveDone!: () => void;
    let rejectDone!: (error: Error) => void;
    const done = new Promise<void>((resolve, reject) => {
      resolveDone = resolve;
      rejectDone = reject;
    });
    const off = this.manager.onSessionEvent(sessionId, (ev: ActorEvent) => {
      switch (ev.type) {
        case 'agent_text':
          progress.report(new vscode.LanguageModelTextPart(ev.text));
          break;
        case 'agent_thinking':
          // The stable response-part union predates the thinking proposal;
          // the proposed part is streamed through the same report channel.
          progress.report(
            new vscode.LanguageModelThinkingPart(ev.text) as unknown as vscode.LanguageModelResponsePart,
          );
          break;
        case 'turn_finished':
          // A cancelled turn is a normal end: the user asked to stop.
          if (settled) return;
          settled = true;
          off();
          cancelSub.dispose();
          resolveDone();
          break;
        case 'error':
          if (settled) return;
          settled = true;
          off();
          cancelSub.dispose();
          rejectDone(new Error(ev.message));
          break;
      }
    });
    const cancelSub = token.onCancellationRequested(() => {
      this.manager.send({ cmd: 'cancel_turn', sessionId });
    });

    try {
      // The user may switch models mid-conversation; re-assert each turn.
      this.manager.send({ cmd: 'set_model', sessionId, id: model.id });
      this.manager.send({
        cmd: 'submit',
        sessionId,
        text,
        ...(images.length > 0 ? { images } : {}),
      });
      await done;
    } finally {
      if (!settled) {
        settled = true;
        off();
        cancelSub.dispose();
      }
      // The session stays cached for the next turn; eviction or extension
      // shutdown releases it.
    }
  }

  async provideTokenCount(
    _model: vscode.LanguageModelChatInformation,
    text: string | vscode.LanguageModelChatRequestMessage,
    _token: vscode.CancellationToken,
  ): Promise<number> {
    const source = typeof text === 'string' ? text : serializeTranscript([text]);
    return Math.ceil(source.length / 4);
  }

  /** Drop idle sessions past the TTL, then evict least-recently-used entries
   * down to the cap. */
  private evictStale(): void {
    const now = Date.now();
    for (const [key, entry] of this.sessions) {
      if (now - entry.lastUsed > SESSION_IDLE_TTL_MS) {
        this.manager.disposeSession(entry.sessionId);
        this.sessions.delete(key);
      }
    }
    while (this.sessions.size >= MAX_SESSIONS) {
      let oldestKey: string | null = null;
      let oldestUsed = Infinity;
      for (const [key, entry] of this.sessions) {
        if (entry.lastUsed < oldestUsed) {
          oldestUsed = entry.lastUsed;
          oldestKey = key;
        }
      }
      if (oldestKey === null) break;
      const evicted = this.sessions.get(oldestKey)!;
      this.manager.disposeSession(evicted.sessionId);
      this.sessions.delete(oldestKey);
    }
  }
}
