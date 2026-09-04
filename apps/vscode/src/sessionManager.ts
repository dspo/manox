// Session multiplexer over the shared transport. Routes session-tagged
// events to per-session subscribers and global events (ready, models,
// untagged errors) to manager-level subscribers, so host surfaces like the
// chat participant and the sidebar each own their session and never see one
// another's turns.
//
// T9 (v2 frames): the manager also exposes `onFrame`, a raw relay that sees
// every `FromServer` envelope (notification / request / response / host /
// streamItem / streamEnd) with zero filtering — the sidebar webview renderer
// consumes the v2 stream frames through it. `host` frames are additionally
// folded into the legacy global-event mirror (§D.5) so the manager's own
// awaits (`ready` / `models` / `threadsUpdated` / `commands`) keep working
// whichever envelope the server uses.

import { EventEmitter } from 'node:events';
import { randomUUID } from 'node:crypto';
import * as vscode from 'vscode';
import type {
  ApprovalMode,
  CommandEntry,
  FromClient,
  FromServer,
  HostEvent,
  ModelInfo,
  ThreadInfoSnapshot,
  ThreadListItem,
} from '../dist/protocol';
import { isSessionEvent, notification, request } from './protocolHelpers';
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

/** Configured tool-authorization policy; unset falls back to workspace-write. */
export function configuredApprovalMode(): ApprovalMode {
  const value = vscode.workspace.getConfiguration('manox').get<string>('approvalMode');
  return value === 'read-only' || value === 'danger-full-access' ? value : 'workspace-write';
}

export class SessionManager {
  private static instance: SessionManager | null = null;
  /** Host identity pinned on the Initialize handshake (§D.2). Minted once
   * per machine by `activate` (persisted in globalState) and replayed on
   * every manager (re)build, so window reloads and re-inits re-seat the
   * server-side owner instead of registering a fresh client. */
  private static clientId = '';

  /** Process-wide manager: the agent server is shared by every host surface.
   * `clientId` is captured on the call that creates the instance (extension
   * activation passes the persisted identity; later calls just hand back the
   * live manager). */
  static shared(clientId?: string): SessionManager {
    if (clientId) SessionManager.clientId = clientId;
    if (!SessionManager.instance) {
      SessionManager.instance = new SessionManager(
        NapiTransport.load(SessionManager.clientId),
      );
    }
    return SessionManager.instance;
  }

  /** Tear down the shared transport if one exists; used from `deactivate`. */
  static async disposeShared(): Promise<void> {
    const instance = SessionManager.instance;
    SessionManager.instance = null;
    if (instance) await instance.dispose();
  }

  private readonly sessions = new Map<string, EventEmitter>();
  private readonly global = new EventEmitter();
  /** Raw `FromServer` relay (T9): every frame, unfiltered, in arrival order. */
  private readonly frames = new EventEmitter();
  /** §D.5 mirror of the thread registry: the latest full list seen on either
   * envelope (legacy `threadsUpdated` note or `HostEvent::ThreadsUpdated`),
   * with `sessionStatus` deltas merged. Seeds the `listThreads` await when
   * the server has moved to host frames for registry pushes. */
  private threadsSnapshot: ThreadListItem[] | null = null;
  private initPhase: 'idle' | 'starting' | 'ready' = 'idle';
  private readyPromise: Promise<void> | null = null;
  private approvalMode: ApprovalMode = configuredApprovalMode();
  /** Map of request id → resolve/reject for outstanding ClientCall responses. */
  private readonly pendingRequests = new Map<string, { resolve: (v: unknown) => void; reject: (e: Error) => void; timeout: NodeJS.Timeout }>();

  private constructor(private readonly transport: Transport) {
    this.global.setMaxListeners(0);
    this.frames.setMaxListeners(0);
    transport.onEvent((msg) => {
      // Raw relay first: the webview-facing surfaces see every frame,
      // unfiltered (T9 pass-through), before host-internal routing.
      this.frames.emit('frame', msg);
      this.route(msg);
    });
  }

  private route(msg: FromServer): void {
    switch (msg.kind) {
      case 'notification': {
        const note = msg.note as { method: string; [key: string]: unknown };
        // Map ServerNote method names to the legacy event type names used by
        // the rest of the codebase. The wire uses camelCase (from Rust serde);
        // the internal dispatch uses the same method-based routing.
        if (note.method === 'sessionCreated' || note.method === 'sessionDisposed') {
          const sessionId = note.sessionId as string;
          if (note.method === 'sessionDisposed') {
            this.sessions.get(sessionId)?.emit('event', note);
            this.sessions.delete(sessionId);
            return;
          }
          this.sessions.get(sessionId)?.emit('event', note);
          return;
        }
        if (isSessionEvent(note as { sessionId?: string | null })) {
          this.sessions.get(note.sessionId as string)?.emit('event', note);
          return;
        }
        if (note.method === 'ready') {
          this.initPhase = 'ready';
        }
        if (note.method === 'threadsUpdated') {
          // Keep the §D.5 mirror in sync while the dual-protocol server
          // still pushes the legacy note.
          this.threadsSnapshot =
            (note.threads as ThreadListItem[] | undefined) ?? this.threadsSnapshot;
        }
        this.global.emit('event', note);
        return;
      }
      case 'request': {
        // ServerCall: adjudication / capability calls. The TS side routes
        // these to the session subscribers.
        const call = msg.call;
        if (call.method === 'approve' || call.method === 'askUserQuestion' || call.method === 'planVerdict') {
          // Forward the ServerCall as a session event so the handler (sidebar
          // or participant) can answer via fromClientReply().
          const sessionId = 'sessionId' in call ? (call as { sessionId: string }).sessionId : undefined;
          if (sessionId) {
            // Emit as a special event type the handlers know to look for.
            this.sessions.get(sessionId)?.emit('serverCall', { id: msg.id, call });
          }
        }
        return;
      }
      case 'response': {
        // Response to a ClientCall request. Resolve the pending request.
        const id = msg.id;
        const pending = this.pendingRequests.get(id);
        if (pending) {
          clearTimeout(pending.timeout);
          this.pendingRequests.delete(id);
          if ('Ok' in msg.outcome) {
            pending.resolve(msg.outcome.Ok as Record<string, unknown>);
          } else {
            pending.reject(new Error(`request failed: ${msg.outcome.Err.message}`));
          }
        }
        return;
      }
      case 'host': {
        // §D.5 host vocabulary: mirror into the legacy global-event shape so
        // the manager's own awaits keep resolving whichever envelope the
        // server uses. The webview receives the frame verbatim via onFrame.
        this.foldHostEvent(msg.host);
        return;
      }
      case 'streamItem':
      case 'streamEnd': {
        // v2 journal frames target the webview renderer (the shared v2
        // bundle routes them by streamId into its journal engine). The host
        // never consumes or filters them; the onFrame relay forwards them.
        return;
      }
    }
  }

  /** Fold a `HostEvent` into the manager's global-event mirror. Unknown
   * future variants are dropped (the raw relay still forwards them). */
  private foldHostEvent(host: HostEvent): void {
    switch (host.type) {
      case 'ready':
        this.initPhase = 'ready';
        this.global.emit('event', { method: 'ready' });
        return;
      case 'models':
        this.global.emit('event', { method: 'models', models: host.models });
        return;
      case 'commands':
        this.global.emit('event', { method: 'commands', commands: host.commands });
        return;
      case 'threadsUpdated':
        this.threadsSnapshot = host.threads;
        this.global.emit('event', { method: 'threadsUpdated', threads: host.threads });
        return;
      case 'sessionStatus': {
        // Merge the per-session projection into the registry mirror. Before
        // any full list has been seen, synthesize a partial entry so the
        // `listThreads` await can resolve; a later `threadsUpdated` (either
        // envelope) replaces the whole snapshot.
        const status: Partial<ThreadListItem> = {
          id: host.sessionId,
          running: host.running ?? false,
          errored: host.errored ?? false,
          unread: host.unread ?? false,
          pending_auth: host.pendingAuth ?? false,
          pending_plan: host.pendingPlan ?? false,
          background_work: host.backgroundWork ?? false,
        };
        if (this.threadsSnapshot) {
          this.threadsSnapshot = this.threadsSnapshot.map((t) =>
            t.id === host.sessionId ? { ...t, ...status } : t,
          );
        } else {
          this.threadsSnapshot = [
            {
              id: host.sessionId,
              title: '',
              updated_at: 0,
              pinned: false,
              archived: false,
              parent_id: null,
              depth: 0,
              model_id: '',
              ...status,
            } as ThreadListItem,
          ];
        }
        this.global.emit('event', {
          method: 'threadsUpdated',
          threads: this.threadsSnapshot,
        });
        return;
      }
      case 'error':
        this.global.emit('event', {
          method: 'error',
          sessionId: null,
          message: host.message,
        });
        return;
      case 'sessionCreated':
      case 'sessionDisposed': {
        const note = { method: host.type, sessionId: host.sessionId };
        const emitter = this.sessions.get(host.sessionId);
        if (emitter) emitter.emit('event', note);
        else this.global.emit('event', note);
        if (host.type === 'sessionDisposed') this.sessions.delete(host.sessionId);
        return;
      }
    }
  }

  /** Initialize the manager. The transport's start() already sends the
   * Initialize handshake; this method waits for the Ready notification. */
  init(_cwd?: string): Promise<void> {
    if (this.initPhase === 'ready') return Promise.resolve();
    if (this.initPhase === 'starting') return this.readyPromise!;
    this.initPhase = 'starting';
    this.readyPromise = (async () => {
      try {
        await this.transport.ready;
        const ready = this.awaitGlobal(
          (ev) => ev.method === 'ready',
          'init ready',
          INIT_TIMEOUT_MS,
        );
        await ready;
      } catch (e) {
        this.initPhase = 'idle';
        this.readyPromise = null;
        throw e;
      }
    })();
    return this.readyPromise;
  }

  /** Create a session and resolve once the server confirms it. */
  async createSession(cwd: string, sessionId: string = randomUUID()): Promise<string> {
    const emitter = new EventEmitter();
    emitter.setMaxListeners(0);
    this.sessions.set(sessionId, emitter);
    const created = this.awaitSession(
      sessionId,
      (ev) => ev.method === 'sessionCreated',
      'session_created',
    );
    this.send(notification({
      method: 'createSession',
      sessionId,
      cwd,
    }));
    try {
      await created;
    } catch (e) {
      this.sessions.delete(sessionId);
      this.send(notification({
        method: 'disposeSession',
        sessionId,
      }));
      throw e;
    }
    // Fresh threads start on the server's default policy; enforce the host's.
    this.send(notification({
      method: 'setApprovalMode',
      sessionId,
      mode: this.approvalMode,
    }));
    return sessionId;
  }

  /** Open a persisted thread as a session and resolve once the server
   * confirms. The restored thread keeps its persisted approval mode. */
  async openThread(sessionId: string): Promise<string> {
    const emitter = new EventEmitter();
    emitter.setMaxListeners(0);
    this.sessions.set(sessionId, emitter);
    const created = this.awaitSession(
      sessionId,
      (ev) => ev.method === 'sessionCreated',
      'session_created',
    );
    // OpenSession is a ClientCall (request), not a notification.
    const openRequest = request({
      method: 'openSession',
      sessionId,
    });
    this.send(openRequest);
    try {
      await created;
    } catch (e) {
      this.sessions.delete(sessionId);
      this.send(notification({
        method: 'disposeSession',
        sessionId,
      }));
      throw e;
    }
    return sessionId;
  }

  /** Change the approval policy and push it to every live session. */
  setApprovalMode(mode: ApprovalMode): void {
    this.approvalMode = mode;
    for (const sessionId of this.sessions.keys()) {
      this.send(notification({
        method: 'setApprovalMode',
        sessionId,
        mode,
      }));
    }
  }

  /**
   * Ask the server to tear a session down. The emitter is removed when the
   * `sessionDisposed` confirmation arrives, not here.
   */
  disposeSession(sessionId: string): void {
    this.send(notification({
      method: 'disposeSession',
      sessionId,
    }));
  }

  fromClientReply(msg: FromClient): void {
    this.transport.send(JSON.stringify(msg));
  }

  send(msg: FromClient): void {
    this.transport.send(JSON.stringify(msg));
  }

  /** Subscribe to one session's events; returns an unsubscribe function. */
  onSessionEvent(sessionId: string, handler: (ev: Record<string, unknown>) => void): () => void {
    let emitter = this.sessions.get(sessionId);
    if (!emitter) {
      emitter = new EventEmitter();
      emitter.setMaxListeners(0);
      this.sessions.set(sessionId, emitter);
    }
    emitter.on('event', handler);
    return () => emitter!.off('event', handler);
  }

  /** T9 raw frame relay: subscribe to every `FromServer` envelope with zero
   * filtering (v2 `streamItem` / `streamEnd` / `host` included). The sidebar
   * webview host forwards these verbatim to the shared v2 bundle. Returns an
   * unsubscribe function. */
  onFrame(handler: (msg: FromServer) => void): () => void {
    this.frames.on('frame', handler);
    return () => this.frames.off('frame', handler);
  }

  /** Subscribe to ServerCall requests for a session (approve/askUserQuestion/planVerdict). */
  onSessionServerCall(sessionId: string, handler: (ev: { id: string; call: Record<string, unknown> }) => void): () => void {
    const emitter = this.sessions.get(sessionId);
    if (!emitter) return () => {};
    emitter.on('serverCall', handler);
    return () => emitter!.off('serverCall', handler);
  }

  /** Subscribe to global events (ready, models, untagged errors). */
  onGlobalEvent(handler: (ev: Record<string, unknown>) => void): () => void {
    this.global.on('event', handler);
    return () => this.global.off('event', handler);
  }

  listModels(): Promise<ModelInfo[]> {
    const models = this.awaitGlobal((ev) => ev.method === 'models', 'models', INIT_TIMEOUT_MS);
    this.send(request({ method: 'listModels' }));
    return models.then((ev) => (ev as Record<string, unknown> & { models: ModelInfo[] }).models);
  }

  listThreads(): Promise<ThreadListItem[]> {
    const threads = this.awaitNote('threadsUpdated', 'threads_updated');
    this.send(request({ method: 'listThreads' }));
    return threads.then(
      (ev) => (ev as Record<string, unknown> & { threads: ThreadListItem[] }).threads,
    );
  }

  archiveThread(sessionId: string, archived: boolean): void {
    this.send(notification({
      method: 'archiveThread',
      sessionId,
      archived,
    }));
  }

  pinThread(sessionId: string, pinned: boolean): void {
    this.send(notification({
      method: 'pinThread',
      sessionId,
      pinned,
    }));
  }

  listCommands(): Promise<CommandEntry[]> {
    const commands = this.awaitGlobal((ev) => ev.method === 'commands', 'commands');
    this.send(request({ method: 'listCommands' }));
    return commands.then(
      (ev) => (ev as Record<string, unknown> & { commands: CommandEntry[] }).commands,
    );
  }

  requestThreadInfo(sessionId: string): Promise<ThreadInfoSnapshot> {
    const info = this.awaitSession(
      sessionId,
      (ev) => ev.method === 'threadInfo',
      'thread_info',
    );
    this.send(request({ method: 'threadInfo', sessionId }));
    return info.then(
      (ev) => (ev as Record<string, unknown> & { info: ThreadInfoSnapshot }).info,
    );
  }

  /** Shut the transport down; `disposeShared` is the public entry point. */
  private async dispose(): Promise<void> {
    for (const [, timeout] of this.pendingRequests) {
      clearTimeout(timeout.timeout);
    }
    this.pendingRequests.clear();
    this.sessions.clear();
    this.frames.removeAllListeners();
    await this.transport.dispose();
  }

  private awaitGlobal(
    match: (ev: Record<string, unknown>) => boolean,
    label: string,
    timeoutMs: number = RESPONSE_TIMEOUT_MS,
  ): Promise<Record<string, unknown>> {
    return this.awaitOn(this.global, match, label, timeoutMs);
  }

  /** Await a global event by method name, short-circuiting from the §D.5
   * host mirror when it already holds the payload (the server's registry
   * pushes may arrive as `HostEvent` frames rather than legacy notes; the
   * mirror is folded by both envelopes). */
  private awaitNote(
    method: string,
    label: string,
    timeoutMs: number = RESPONSE_TIMEOUT_MS,
  ): Promise<Record<string, unknown>> {
    if (method === 'threadsUpdated' && this.threadsSnapshot) {
      return Promise.resolve({ method, threads: this.threadsSnapshot });
    }
    return this.awaitGlobal((ev) => ev.method === method, label, timeoutMs);
  }

  private awaitSession(
    sessionId: string,
    match: (ev: Record<string, unknown>) => boolean,
    label: string,
  ): Promise<Record<string, unknown>> {
    const emitter = this.sessions.get(sessionId);
    if (!emitter) return Promise.reject(new Error(`unknown session: ${sessionId}`));
    return this.awaitOn(emitter, match, label);
  }

  private awaitOn(
    emitter: EventEmitter,
    match: (ev: Record<string, unknown>) => boolean,
    label: string,
    timeoutMs: number = RESPONSE_TIMEOUT_MS,
  ): Promise<Record<string, unknown>> {
    return new Promise((resolve, reject) => {
      const handler = (ev: Record<string, unknown>) => {
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