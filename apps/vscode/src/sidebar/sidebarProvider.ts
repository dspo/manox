// Sidebar host: bridges the webview renderer to the shared agent server. The
// webview is a pure renderer and a thread list is its home screen — nothing
// here infers a "current" session. Every per-thread command arrives with
// its sessionId, live sessions are kept in a registry (view switching never
// disposes), and disposal happens only at sidebar teardown.

import * as crypto from 'node:crypto';
import * as vscode from 'vscode';
import type { FromServer, ImageAttachment, ModelInfo, ServerNote, ThreadInfoSnapshot, ThreadListItem, CommandEntry } from '../../dist/protocol';
import { isSessionEvent } from '../../dist/protocol';
import { notification, request, reply } from '../protocolHelpers';
import { SessionManager, resolveWorkspaceCwd } from '../sessionManager';
import type { HostToWebview, WebviewToHost } from '../../dist/sidebar/messages';

/** Frame interval for draining the session-event buffer. One postMessage
 * per interval carries every buffered event, so a streaming turn (one wire
 * event per token / stdout chunk) crosses the bridge ~30 times a second at
 * most instead of hundreds of times. */
const EVENT_BATCH_INTERVAL_MS = 33;

export function registerManoxSidebar(context: vscode.ExtensionContext): void {
  const provider = new ManoxSidebarProvider(context.extensionUri);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider('manox.chatView', provider, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
    vscode.commands.registerCommand('manox.focus', () =>
      vscode.commands.executeCommand('manox.chatView.focus'),
    ),
    vscode.commands.registerCommand('manox.newSession', () => provider.newSession()),
    // macOS cmd+m is a minimize accelerator; the extension keybinding
    // contribution routes it here instead, since the webview DOM would
    // never receive the key.
    vscode.commands.registerCommand('manox.openTurnNavigator', () =>
      provider.openTurnNavigator(),
    ),
  );
}

class ManoxSidebarProvider implements vscode.WebviewViewProvider {
  private view: vscode.WebviewView | null = null;
  /** Live sessions: unsubscribe per session id. View switching never
   * removes entries — only teardown does. */
  private readonly sessions = new Map<string, () => void>();
  private unsubscribeGlobal: (() => void) | null = null;
  /** Monotonic token invalidated by teardown: an in-flight create/open
   * completing afterwards disposes its actor-side session instead of
   * attaching to a dead view. */
  private sessionGeneration = 0;
  /** Session events waiting for the next batched flush. Arrival order is
   * preserved across sessions; bypass messages (session_ready, thread_info)
   * drain the buffer first so they never overtake queued events. */
  private pendingEvents: Record<string, unknown>[] = [];
  private flushTimer: NodeJS.Timeout | null = null;

  constructor(private readonly extensionUri: vscode.Uri) {}

  /** Toggle the turn navigator from the host (macOS cmd+m path). */
  openTurnNavigator(): void {
    this.post({ type: 'open_turn_navigator' });
  }

  resolveWebviewView(webviewView: vscode.WebviewView): void {
    this.view = webviewView;
    webviewView.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, 'dist')],
    };
    webviewView.webview.html = renderHtml(webviewView.webview, this.extensionUri);

    webviewView.webview.onDidReceiveMessage((msg: WebviewToHost) => {
      void this.onWebviewMessage(msg);
    });
    webviewView.onDidDispose(() => this.teardown());

    this.unsubscribeGlobal = SessionManager.shared().onGlobalEvent((ev) => {
      switch (ev.method) {
        case 'error':
          this.post({ type: 'global_error', message: ev.message as string });
          return;
        case 'models':
          // The server pushes a snapshot once provider registration lands;
          // relays keep the picker live without a re-request.
          this.post({ type: 'models', models: ev.models as [] });
          return;
        case 'threadsUpdated':
          this.post({ type: 'threads', threads: ev.threads as [] });
          return;
        case 'commands':
          this.post({ type: 'commands', commands: ev.commands as [] });
          return;
      }
    });

    void this.initActor();
  }

  /** Create a fresh thread and hand it to the webview (manox.newSession).
   * A payload turns this into the home-composer flow: the webview picks the
   * id so its optimistic draft and the actor session agree, and the first
   * message is submitted right after creation. */
  async newSession(opts?: {
    sessionId?: string;
    text?: string;
    images?: ImageAttachment[];
    modelId?: string;
  }): Promise<void> {
    const generation = ++this.sessionGeneration;
    const manager = SessionManager.shared();
    const cwd = resolveWorkspaceCwd();
    try {
      await manager.init(cwd);
      const sessionId = await manager.createSession(cwd, opts?.sessionId);
      if (generation !== this.sessionGeneration) {
        manager.disposeSession(sessionId);
        return;
      }
      this.registerSession(sessionId, 'fresh', cwd);
      if (opts?.modelId) {
        manager.send(notification({
          method: 'setModel',
          sessionId,
          id: opts.modelId,
        }));
      }
      if (opts?.text || opts?.images?.length) {
        manager.send(notification({
          method: 'submit',
          sessionId,
          text: opts.text ?? '',
          images: opts.images ?? [],
          clientId: null,
        }));
      }
    } catch (e) {
      this.post({
        type: 'global_error',
        message: `manox core unavailable: ${e instanceof Error ? e.message : String(e)}`,
      });
    }
  }

  private async initActor(): Promise<void> {
    try {
      await SessionManager.shared().init(resolveWorkspaceCwd());
    } catch (e) {
      this.post({
        type: 'global_error',
        message: `manox core unavailable: ${e instanceof Error ? e.message : String(e)}`,
      });
    }
  }

  private async openThread(sessionId: string): Promise<void> {
    const manager = SessionManager.shared();
    if (this.sessions.has(sessionId)) {
      // Already live: re-announce the session so a reloaded webview can
      // rebuild its state from scratch, then have the server replay its
      // history/info snapshots through the existing subscription. The flush
      // keeps any buffered events ahead of the re-announcement, where they
      // still fold into the pre-replay state.
      this.flushEvents();
      this.post({ type: 'session_ready', sessionId, cwd: resolveWorkspaceCwd(), kind: 'restored' });
      manager.send(notification({
        method: 'focusThread',
        sessionId: null,
      }));
      manager.send(request({ method: 'openSession', sessionId }));
      return;
    }
    const generation = this.sessionGeneration;
    const cwd = resolveWorkspaceCwd();
    try {
      await manager.init(cwd);
      await manager.openThread(sessionId);
      if (generation !== this.sessionGeneration) {
        manager.disposeSession(sessionId);
        return;
      }
      this.registerSession(sessionId, 'restored', cwd);
    } catch (e) {
      this.post({
        type: 'global_error',
        message: `manox core unavailable: ${e instanceof Error ? e.message : String(e)}`,
      });
    }
  }

  /** Attach the view to a confirmed session: event forwarding plus the
   * model snapshot the composer renders. */
  private registerSession(sessionId: string, kind: 'fresh' | 'restored', cwd: string): void {
    const manager = SessionManager.shared();
    // The ready marker precedes the subscription so the webview always holds
    // thread state for the session before its first event folds in.
    this.post({ type: 'session_ready', sessionId, cwd, kind });
    const unsubscribe = manager.onSessionEvent(sessionId, (ev: Record<string, unknown>) => {
      if (ev.method === 'sessionDisposed') {
        // The server-side session is gone; drop the stale subscription entry
        // so the host map stops growing across /exit and archive cycles.
        this.sessions.delete(sessionId);
      }
      if (ev.method === 'threadInfo') {
        // The snapshot bypasses the buffer; drain it first so it never
        // overtakes earlier events.
        this.flushEvents();
        this.post({ type: 'thread_info', sessionId: ev.sessionId as string, info: ev.info as unknown as ThreadInfoSnapshot });
        return;
      }
      this.queueEvent(ev);
    });
    this.sessions.set(sessionId, unsubscribe);
    manager.send(notification({ method: 'setModel', sessionId, id: '' }));
  }

  /** Buffer one session event for the next batched flush; arms the flush
   * timer on the first queued event of a frame. */
  private queueEvent(ev: Record<string, unknown>): void {
    this.pendingEvents.push(ev);
    if (this.flushTimer === null) {
      this.flushTimer = setTimeout(() => this.flushEvents(), EVENT_BATCH_INTERVAL_MS);
    }
  }

  /** Drain the buffer as a single `events` message. Idempotent; bypass
   * messages and teardown call this to keep the wire order intact. */
  private flushEvents(): void {
    if (this.flushTimer !== null) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    if (this.pendingEvents.length === 0) return;
    const events = this.pendingEvents;
    this.pendingEvents = [];
    this.post({ type: 'events', events });
  }

  private async onWebviewMessage(msg: WebviewToHost): Promise<void> {
    const manager = SessionManager.shared();
    switch (msg.type) {
      case 'submit':
        manager.send(notification({
          method: 'submit',
          sessionId: msg.sessionId,
          text: msg.text,
          images: msg.images ?? [],
          clientId: msg.clientId ?? null,
        }));
        return;
      case 'steer':
        manager.send(notification({
          method: 'steer',
          sessionId: msg.sessionId,
          clientId: msg.clientId,
          text: msg.text,
          images: msg.images ?? [],
        }));
        return;
      case 'drop_queued':
        manager.send(notification({
          method: 'dropQueued',
          sessionId: msg.sessionId,
          clientId: msg.clientId,
        }));
        return;
      case 'approve':
        // The approval dialog sends a reply to the ServerCall::Approve.
        // The server call id is not available here; the sidebar tracks
        // pending approvals via the session event stream.
        manager.send(notification({
          method: 'setApprovalMode',
          sessionId: msg.sessionId,
          mode: msg.allow ? 'workspace-write' : 'read-only',
        }));
        return;
      case 'cancel':
        manager.send(notification({
          method: 'cancelTurn',
          sessionId: msg.sessionId,
        }));
        return;
      case 'set_model':
        manager.send(notification({
          method: 'setModel',
          sessionId: msg.sessionId,
          id: msg.id,
        }));
        return;
      case 'set_reasoning_effort':
        manager.send(notification({
          method: 'setReasoningEffort',
          sessionId: msg.sessionId,
          effort: msg.effort,
        }));
        return;
      case 'set_approval_mode':
        manager.send(notification({
          method: 'setApprovalMode',
          sessionId: msg.sessionId,
          mode: msg.mode,
        }));
        return;
      case 'set_plan_mode':
        manager.send(notification({
          method: 'setPlanMode',
          sessionId: msg.sessionId,
          enabled: msg.enabled,
        }));
        return;
      case 'plan_verdict':
        // The verdict choice is sent as a compact notification. The server
        // will compact the session according to the choice.
        manager.send(notification({
          method: 'compact',
          sessionId: msg.sessionId,
          instructions: null,
        }));
        return;
      case 'plan_execute_fresh': {
        void (async () => {
          const generation = ++this.sessionGeneration;
          try {
            await manager.init(resolveWorkspaceCwd());
            manager.send(notification({
              method: 'archiveThread',
              sessionId: msg.sessionId,
              archived: true,
            }));
            const freshId = crypto.randomUUID();
            await manager.createSession(msg.cwd, freshId);
            if (generation !== this.sessionGeneration) {
              manager.disposeSession(freshId);
              return;
            }
            this.registerSession(freshId, 'fresh', msg.cwd);
            manager.send(notification({
              method: 'planSeedExecution',
              sessionId: freshId,
              planFile: msg.planFile,
            }));
          } catch (e) {
            this.post({ type: 'global_error', message: String(e) });
          }
        })();
        return;
      }
      case 'goal':
        manager.send(notification({
          method: 'goal',
          sessionId: msg.sessionId,
          action: msg.action,
          objective: msg.objective ?? null,
          budget: (msg.budget ?? null) as unknown as bigint | null,
          maxRounds: null,
        }));
        return;
      case 'stop_background_task':
        manager.send(notification({
          method: 'stopBackgroundTask',
          sessionId: msg.sessionId,
          taskId: msg.taskId,
        }));
        return;
      case 'answer_question':
        manager.send(notification({
          method: 'compact',
          sessionId: msg.sessionId,
          instructions: null,
        }));
        return;
      case 'request_usage':
        manager.send(request({ method: 'getUsage', sessionId: msg.sessionId }));
        return;
      case 'request_thread_info':
        manager.send(request({ method: 'threadInfo', sessionId: msg.sessionId }));
        return;
      case 'focus_thread':
        manager.send(notification({
          method: 'focusThread',
          sessionId: msg.sessionId ?? null,
        }));
        return;
      case 'request_models':
        try {
          await manager.init(resolveWorkspaceCwd());
          this.post({ type: 'models', models: await manager.listModels() as ModelInfo[] });
        } catch (e) {
          this.post({ type: 'global_error', message: String(e) });
        }
        return;
      case 'list_threads':
        try {
          await manager.init(resolveWorkspaceCwd());
          manager.send(request({ method: 'listThreads' }));
        } catch (e) {
          this.post({ type: 'global_error', message: String(e) });
        }
        return;
      case 'list_commands':
        try {
          await manager.init(resolveWorkspaceCwd());
          manager.send(request({ method: 'listCommands' }));
        } catch (e) {
          this.post({ type: 'global_error', message: String(e) });
        }
        return;
      case 'open_thread':
        await this.openThread(msg.sessionId);
        return;
      case 'archive_thread':
        manager.send(notification({
          method: 'archiveThread',
          sessionId: msg.sessionId,
          archived: msg.archived,
        }));
        return;
      case 'pin_thread':
        manager.send(notification({
          method: 'pinThread',
          sessionId: msg.sessionId,
          pinned: msg.pinned,
        }));
        return;
      case 'new_session':
        await this.newSession({
          sessionId: msg.sessionId,
          text: msg.text,
          images: msg.images,
          modelId: msg.modelId,
        });
        return;
    }
  }

  private teardown(): void {
    this.sessionGeneration++;
    if (this.flushTimer !== null) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    this.pendingEvents = [];
    const manager = SessionManager.shared();
    for (const [sessionId, unsubscribe] of this.sessions) {
      unsubscribe();
      manager.disposeSession(sessionId);
    }
    this.sessions.clear();
    this.unsubscribeGlobal?.();
    this.unsubscribeGlobal = null;
    this.view = null;
  }

  private post(message: HostToWebview): void {
    void this.view?.webview.postMessage(message);
  }
}

function renderHtml(webview: vscode.Webview, extensionUri: vscode.Uri): string {
  const nonce = crypto.randomBytes(16).toString('base64');
  const scriptUri = webview.asWebviewUri(
    vscode.Uri.joinPath(extensionUri, 'dist', 'webview', 'bundle.js'),
  );
  const styleUri = webview.asWebviewUri(
    vscode.Uri.joinPath(extensionUri, 'dist', 'webview', 'bundle.css'),
  );
  return /* html */ `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="vscode-language" content="${vscode.env.language}">
  <meta http-equiv="Content-Security-Policy"
    content="default-src 'none'; script-src ${webview.cspSource} 'nonce-${nonce}'; img-src ${webview.cspSource} data:; style-src ${webview.cspSource} 'nonce-${nonce}'; font-src ${webview.cspSource};">
  <link rel="stylesheet" href="${styleUri}">
</head>
<body>
  <div id="root"></div>
  <script nonce="${nonce}" src="${scriptUri}"></script>
</body>
</html>`;
}