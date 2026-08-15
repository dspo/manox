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
  /** A turn is in flight; running entries are exempt from eviction. */
  running: boolean;
}

/** Role ordinal → label used in the transcript seed projection. */
const ROLE_NAMES: Record<number, string> = { 1: 'User', 2: 'Assistant', 3: 'System' };

/**
 * Conversation identity: a hash of the first user message's text parts.
 * Appended turns keep the key stable; editing that first message starts a
 * fresh conversation (and loses the cached session's memory). The provider
 * API exposes no conversation id, so conversations that open with identical
 * text (or with an image-only first message, which hashes as empty) share
 * one session — a documented limitation.
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

/** Index of the last user message, or -1 when none exists. */
function lastUserIndex(
  messages: readonly vscode.LanguageModelChatRequestMessage[],
): number {
  for (let i = messages.length - 1; i >= 0; i--) {
    if (messages[i].role === vscode.LanguageModelChatMessageRole.User) return i;
  }
  return -1;
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
  const index = lastUserIndex(messages);
  if (index < 0) return { text: '', images: [] };
  const text: string[] = [];
  const images: ImageAttachment[] = [];
  for (const part of messages[index].content) {
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

/**
 * Stream one thinking chunk. The thinking part is a proposed API that only
 * hosts declaring the proposal inject; stable hosts fall back to a text part
 * so reasoning still streams instead of throwing in the event callback.
 */
export function reportThinking(
  progress: vscode.Progress<vscode.LanguageModelResponsePart>,
  text: string,
): void {
  const ThinkingPart = (
    vscode as unknown as {
      LanguageModelThinkingPart?: new (value: string) => vscode.LanguageModelResponsePart;
    }
  ).LanguageModelThinkingPart;
  progress.report(ThinkingPart ? new ThinkingPart(text) : new vscode.LanguageModelTextPart(text));
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
        capabilities: { toolCalling: true, imageInput: true },
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
    if (token.isCancellationRequested) return;

    this.evictStale();
    const key = conversationKey(messages);
    const existing = this.sessions.get(key);
    const isNewSession = !existing;
    let sessionId: string;
    let entry: SessionEntry;
    if (existing) {
      if (existing.running) {
        // The host serializes same-conversation requests, so an in-flight
        // hit means two conversations collided on one key. Fail loudly
        // instead of queueing into the actor's running-turn no-op.
        throw new Error(
          'manox session busy: another conversation opened with the same first message',
        );
      }
      existing.lastUsed = Date.now();
      sessionId = existing.sessionId;
      entry = existing;
    } else {
      sessionId = await this.manager.createSession(cwd);
      if (token.isCancellationRequested) {
        this.manager.disposeSession(sessionId);
        return;
      }
      entry = { sessionId, lastUsed: Date.now(), running: false };
      this.sessions.set(key, entry);
    }

    const lastIdx = lastUserIndex(messages);
    const { text: deltaText, images } = extractDelta(messages);
    if (deltaText === '' && images.length === 0) {
      // The actor drops empty submits without a turn_finished; reject
      // instead of leaving the request hanging.
      if (isNewSession) {
        this.manager.disposeSession(sessionId);
        this.sessions.delete(key);
      }
      throw new Error('empty agent request: the last user message has no text or image');
    }
    let text = deltaText;
    if (isNewSession && lastIdx > 0) {
      // Seed the fresh session with everything before the last user message
      // so the agent knows the conversation that led here; later turns only
      // send their delta and continue on the session transcript.
      text = serializeTranscript(messages.slice(0, lastIdx)) + '\n\n---\n\n' + deltaText;
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
          reportThinking(progress, ev.text);
          break;
        case 'turn_finished':
          if (settled) return;
          settled = true;
          off();
          cancelSub.dispose();
          // A cancelled turn is a normal end; a failed turn carries no
          // error event of its own in every path, so reject on the flag.
          if (ev.failed) rejectDone(new Error('agent turn failed'));
          else resolveDone();
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

    entry.running = true;
    try {
      // The user may switch models mid-conversation; re-assert each turn.
      this.manager.send({ cmd: 'set_model', sessionId, id: model.id });
      // The config listener broadcasts the approval mode to every session;
      // re-assert danger so a switch to autopilot cannot stall a turn on an
      // approval the native chat cannot answer.
      this.manager.send({ cmd: 'set_approval_mode', sessionId, mode: 'danger' });
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
      entry.running = false;
      entry.lastUsed = Date.now();
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
   * down to the cap; sessions with an in-flight turn are never evicted. */
  private evictStale(): void {
    const now = Date.now();
    for (const [key, entry] of this.sessions) {
      if (!entry.running && now - entry.lastUsed > SESSION_IDLE_TTL_MS) {
        this.manager.disposeSession(entry.sessionId);
        this.sessions.delete(key);
      }
    }
    while (this.sessions.size >= MAX_SESSIONS) {
      let oldestKey: string | null = null;
      let oldestUsed = Infinity;
      for (const [key, entry] of this.sessions) {
        if (!entry.running && entry.lastUsed < oldestUsed) {
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
