// Session multiplexer over the shared actor transport. Routes session-tagged
// events to per-session subscribers and global events (ready, models,
// untagged errors) to manager-level subscribers, so host surfaces like the
// chat participant and the sidebar each own their session and never see one
// another's turns.

import { EventEmitter } from 'node:events';
import { randomUUID } from 'node:crypto';
import * as vscode from 'vscode';
import type {
  ActorEvent,
  ApprovalMode,
  Command,
  CommandEntry,
  ModelInfo,
  ThreadInfoSnapshot,
  ThreadListItem,
} from './protocol';
import { isSessionEvent } from './protocol';
import type { Transport } from './transport/transport';
import { NapiTransport } from './transport/napiTransport';

const RESPONSE_TIMEOUT_MS = 5_000;
// First init boots the agent runtime (providers, MCP, LSP, plugins) and can
// take much longer than a round trip on a cold machine.
const INIT_TIMEOUT_MS = 30_000;

/** Workspace folder the agent operates on; falls back to HOME, then cwd. */
export function resolveWorkspaceCwd(): string {
  const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  return folder ?? process.env.HOME ?? process.cwd();
}

/** Configured tool-authorization policy; unset falls back to danger. */
export function configuredApprovalMode(): ApprovalMode {
  const value = vscode.workspace.getConfiguration('manox').get<string>('approvalMode');
  return value === 'autopilot' ? 'autopilot' : 'danger';
}

export class SessionManager {
  private static instance: SessionManager | null = null;

  /** Process-wide manager: the actor thread is shared by every host surface. */
  static shared(): SessionManager {
    if (!SessionManager.instance) {
      SessionManager.instance = new SessionManager(NapiTransport.load());
    }
    return SessionManager.instance;
  }

  /** Tear down the shared actor if one exists; used from `deactivate`. */
  static async disposeShared(): Promise<void> {
    const instance = SessionManager.instance;
    SessionManager.instance = null;
    if (instance) await instance.dispose();
  }

  private readonly sessions = new Map<string, EventEmitter>();
  private readonly global = new EventEmitter();
  private initPhase: 'idle' | 'starting' | 'ready' = 'idle';
  private readyPromise: Promise<void> | null = null;
  private approvalMode: ApprovalMode = configuredApprovalMode();

  private constructor(private readonly transport: Transport) {
    this.global.setMaxListeners(0);
    transport.onEvent((ev) => this.route(ev));
  }

  private route(ev: ActorEvent): void {
    if (isSessionEvent(ev)) {
      if (ev.type === 'session_disposed') {
        // Keep the emitter alive until the confirmation lands so late
        // events of an in-flight turn still reach their handlers.
        this.sessions.get(ev.sessionId)?.emit('event', ev);
        this.sessions.delete(ev.sessionId);
        return;
      }
      this.sessions.get(ev.sessionId)?.emit('event', ev);
      return;
    }
    if (ev.type === 'ready' && this.initPhase === 'starting') {
      this.initPhase = 'ready';
    }
    this.global.emit('event', ev);
  }

  /** Initialize the actor with a working directory; idempotent per process. */
  init(cwd: string): Promise<void> {
    if (this.initPhase === 'ready') return Promise.resolve();
    if (this.initPhase === 'starting') return this.readyPromise!;
    this.initPhase = 'starting';
    this.readyPromise = (async () => {
      try {
        await this.transport.ready;
        const ready = this.awaitGlobal(
          (ev) => ev.type === 'ready',
          'init ready',
          INIT_TIMEOUT_MS,
        );
        this.send({ cmd: 'init', cwd, host: 'vscode' });
        await ready;
      } catch (e) {
        this.initPhase = 'idle';
        this.readyPromise = null;
        throw e;
      }
    })();
    return this.readyPromise;
  }

  /** Create a session and resolve once the actor confirms it. The id is
   * generated unless the caller already chose one (a surface rendering an
   * optimistic draft before the session exists). */
  async createSession(cwd: string, sessionId: string = randomUUID()): Promise<string> {
    const emitter = new EventEmitter();
    emitter.setMaxListeners(0);
    this.sessions.set(sessionId, emitter);
    const created = this.awaitSession(
      sessionId,
      (ev) => ev.type === 'session_created',
      'session_created',
    );
    this.send({ cmd: 'create_session', sessionId, cwd });
    try {
      await created;
    } catch (e) {
      this.sessions.delete(sessionId);
      // Reclaim the actor-side session as well: the actor may still process
      // the queued create_session after this side has given up waiting.
      this.send({ cmd: 'dispose_session', sessionId });
      throw e;
    }
    // Fresh threads start on the actor's default policy; enforce the host's.
    this.send({ cmd: 'set_approval_mode', sessionId, mode: this.approvalMode });
    return sessionId;
  }

  /** Open a persisted thread as a session and resolve once the actor
   * confirms. The restored thread keeps its persisted approval mode. */
  async openThread(sessionId: string): Promise<string> {
    const emitter = new EventEmitter();
    emitter.setMaxListeners(0);
    this.sessions.set(sessionId, emitter);
    const created = this.awaitSession(
      sessionId,
      (ev) => ev.type === 'session_created',
      'session_created',
    );
    this.send({ cmd: 'open_thread', sessionId });
    try {
      await created;
    } catch (e) {
      this.sessions.delete(sessionId);
      // Reclaim the actor-side session as well: the actor may still process
      // the queued open_thread after this side has given up waiting.
      this.send({ cmd: 'dispose_session', sessionId });
      throw e;
    }
    return sessionId;
  }

  /** Change the approval policy and push it to every live session. */
  setApprovalMode(mode: ApprovalMode): void {
    this.approvalMode = mode;
    for (const sessionId of this.sessions.keys()) {
      this.send({ cmd: 'set_approval_mode', sessionId, mode });
    }
  }

  /**
   * Ask the actor to tear a session down. The emitter is removed when the
   * `session_disposed` confirmation arrives, not here.
   */
  disposeSession(sessionId: string): void {
    this.send({ cmd: 'dispose_session', sessionId });
  }

  send(command: Command): void {
    this.transport.send(JSON.stringify(command));
  }

  /** Subscribe to one session's events; returns an unsubscribe function. */
  onSessionEvent(sessionId: string, handler: (ev: ActorEvent) => void): () => void {
    let emitter = this.sessions.get(sessionId);
    if (!emitter) {
      emitter = new EventEmitter();
      emitter.setMaxListeners(0);
      this.sessions.set(sessionId, emitter);
    }
    emitter.on('event', handler);
    return () => emitter!.off('event', handler);
  }

  /** Subscribe to global events (ready, models, untagged errors). */
  onGlobalEvent(handler: (ev: ActorEvent) => void): () => void {
    this.global.on('event', handler);
    return () => this.global.off('event', handler);
  }

  listModels(): Promise<ModelInfo[]> {
    // The actor answers only after its one-shot provider registration
    // (keychain/shell lookups) completes; budget the cold boot rather than
    // a plain round trip.
    const models = this.awaitGlobal((ev) => ev.type === 'models', 'models', INIT_TIMEOUT_MS);
    this.send({ cmd: 'list_models' });
    return models.then((ev) => (ev as Extract<ActorEvent, { type: 'models' }>).models);
  }

  /** Snapshot of this project's threads; the actor also pushes
   * `threads_updated` on store mutations via the global event stream. */
  listThreads(): Promise<ThreadListItem[]> {
    const threads = this.awaitGlobal(
      (ev) => ev.type === 'threads_updated',
      'threads_updated',
    );
    this.send({ cmd: 'list_threads' });
    return threads.then(
      (ev) => (ev as Extract<ActorEvent, { type: 'threads_updated' }>).threads,
    );
  }

  /** Archive/unarchive a thread; the store mutation pushes an updated
   * `threads_updated` snapshot through the global stream. */
  archiveThread(sessionId: string, archived: boolean): void {
    this.send({ cmd: 'archive_thread', sessionId, archived });
  }

  /** Pin/unpin a thread; the store mutation pushes an updated
   * `threads_updated` snapshot through the global stream. */
  pinThread(sessionId: string, pinned: boolean): void {
    this.send({ cmd: 'pin_thread', sessionId, pinned });
  }

  /** Slash-completion entries: prompt-macro commands plus skills. */
  listCommands(): Promise<CommandEntry[]> {
    const commands = this.awaitGlobal((ev) => ev.type === 'commands', 'commands');
    this.send({ cmd: 'list_commands' });
    return commands.then((ev) => (ev as Extract<ActorEvent, { type: 'commands' }>).commands);
  }

  /** On-demand conversation info snapshot (worktree, plan, usage, agents). */
  requestThreadInfo(sessionId: string): Promise<ThreadInfoSnapshot> {
    const info = this.awaitSession(
      sessionId,
      (ev) => ev.type === 'thread_info',
      'thread_info',
    );
    this.send({ cmd: 'thread_info', sessionId });
    return info.then((ev) => (ev as Extract<ActorEvent, { type: 'thread_info' }>).info);
  }

  /** Shut the actor down; `disposeShared` is the public entry point. */
  private async dispose(): Promise<void> {
    this.sessions.clear();
    await this.transport.dispose();
  }

  private awaitGlobal(
    match: (ev: ActorEvent) => boolean,
    label: string,
    timeoutMs: number = RESPONSE_TIMEOUT_MS,
  ): Promise<ActorEvent> {
    return this.awaitOn(this.global, match, label, timeoutMs);
  }

  private awaitSession(
    sessionId: string,
    match: (ev: ActorEvent) => boolean,
    label: string,
  ): Promise<ActorEvent> {
    const emitter = this.sessions.get(sessionId);
    if (!emitter) return Promise.reject(new Error(`unknown session: ${sessionId}`));
    return this.awaitOn(emitter, match, label);
  }

  private awaitOn(
    emitter: EventEmitter,
    match: (ev: ActorEvent) => boolean,
    label: string,
    timeoutMs: number = RESPONSE_TIMEOUT_MS,
  ): Promise<ActorEvent> {
    return new Promise((resolve, reject) => {
      const handler = (ev: ActorEvent) => {
        if (!match(ev)) return;
        clearTimeout(timer);
        emitter.off('event', handler);
        resolve(ev);
      };
      const timer = setTimeout(() => {
        emitter.off('event', handler);
        reject(new Error(`timed out waiting for actor event: ${label}`));
      }, timeoutMs);
      emitter.on('event', handler);
    });
  }
}
