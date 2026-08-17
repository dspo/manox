// Sidebar host: bridges the webview renderer to the shared actor. The
// webview is a pure renderer and a thread list is its home screen — nothing
// here infers a "current" session. Every per-thread command arrives with
// its sessionId, live sessions are kept in a registry (view switching never
// disposes), and disposal happens only at sidebar teardown.

import * as crypto from 'node:crypto';
import * as vscode from 'vscode';
import type { ActorEvent, ImageAttachment } from '../protocol';
import { SessionManager, resolveWorkspaceCwd } from '../sessionManager';
import type { HostToWebview, WebviewToHost } from './messages';

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

  constructor(private readonly extensionUri: vscode.Uri) {}

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
      switch (ev.type) {
        case 'error':
          this.post({ type: 'global_error', message: ev.message });
          return;
        case 'models':
          // The actor pushes a snapshot once provider registration lands;
          // relays keep the picker live without a re-request.
          this.post({ type: 'models', models: ev.models });
          return;
        case 'threads_updated':
          this.post({ type: 'threads', threads: ev.threads });
          return;
        case 'commands':
          this.post({ type: 'commands', commands: ev.commands });
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
        manager.send({ cmd: 'set_model', sessionId, id: opts.modelId });
      }
      if (opts?.text || opts?.images?.length) {
        manager.send({ cmd: 'submit', sessionId, text: opts.text ?? '', images: opts.images });
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
      // rebuild its state from scratch, then have the actor replay its
      // history/info snapshots through the existing subscription.
      this.post({ type: 'session_ready', sessionId, cwd: resolveWorkspaceCwd(), kind: 'restored' });
      manager.send({ cmd: 'get_current_model', sessionId });
      manager.send({ cmd: 'open_thread', sessionId });
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
    const unsubscribe = manager.onSessionEvent(sessionId, (ev: ActorEvent) => {
      if (ev.type === 'thread_info') {
        this.post({ type: 'thread_info', sessionId: ev.sessionId, info: ev.info });
        return;
      }
      this.post({ type: 'event', event: ev });
    });
    this.sessions.set(sessionId, unsubscribe);
    this.post({ type: 'session_ready', sessionId, cwd, kind });
    manager.send({ cmd: 'get_current_model', sessionId });
  }

  private async onWebviewMessage(msg: WebviewToHost): Promise<void> {
    const manager = SessionManager.shared();
    switch (msg.type) {
      case 'submit':
        manager.send({ cmd: 'submit', sessionId: msg.sessionId, text: msg.text, images: msg.images });
        return;
      case 'approve':
        manager.send({ cmd: 'approve', sessionId: msg.sessionId, id: msg.id, allow: msg.allow });
        return;
      case 'cancel':
        manager.send({ cmd: 'cancel_turn', sessionId: msg.sessionId });
        return;
      case 'set_model':
        manager.send({ cmd: 'set_model', sessionId: msg.sessionId, id: msg.id });
        return;
      case 'set_reasoning_effort':
        manager.send({ cmd: 'set_reasoning_effort', sessionId: msg.sessionId, effort: msg.effort });
        return;
      case 'set_approval_mode':
        manager.send({ cmd: 'set_approval_mode', sessionId: msg.sessionId, mode: msg.mode });
        return;
      case 'set_plan_mode':
        manager.send({ cmd: 'set_plan_mode', sessionId: msg.sessionId, enabled: msg.enabled });
        return;
      case 'plan_verdict':
        manager.send({ cmd: 'plan_verdict', sessionId: msg.sessionId, choice: msg.choice });
        return;
      case 'plan_execute_fresh': {
        // Execute-fresh orchestration: archive the reviewing session, spin a
        // new one in the same cwd, then seed it with the plan so it starts
        // executing immediately.
        void (async () => {
          try {
            await manager.init(resolveWorkspaceCwd());
            manager.archiveThread(msg.sessionId, true);
            const freshId = crypto.randomUUID();
            await manager.createSession(msg.cwd, freshId);
            manager.send({ cmd: 'plan_seed_execution', sessionId: freshId, planFile: msg.planFile });
          } catch (e) {
            this.post({ type: 'global_error', message: String(e) });
          }
        })();
        return;
      }
      case 'goal':
        manager.send({
          cmd: 'goal',
          sessionId: msg.sessionId,
          action: msg.action,
          objective: msg.objective,
          budget: msg.budget,
        });
        return;
      case 'stop_background_task':
        manager.send({ cmd: 'stop_background_task', sessionId: msg.sessionId, taskId: msg.taskId });
        return;
      case 'answer_question':
        manager.send({
          cmd: 'answer_question',
          sessionId: msg.sessionId,
          id: msg.id,
          answers: msg.answers,
          response: msg.response,
        });
        return;
      case 'request_usage':
        manager.send({ cmd: 'get_usage', sessionId: msg.sessionId });
        return;
      case 'request_thread_info':
        manager.send({ cmd: 'thread_info', sessionId: msg.sessionId });
        return;
      case 'focus_thread':
        manager.send({ cmd: 'focus_thread', sessionId: msg.sessionId });
        return;
      case 'request_models':
        try {
          await manager.init(resolveWorkspaceCwd());
          this.post({ type: 'models', models: await manager.listModels() });
        } catch (e) {
          this.post({ type: 'global_error', message: String(e) });
        }
        return;
      case 'list_threads':
        // The thread store exists only after init; sequence behind it.
        try {
          await manager.init(resolveWorkspaceCwd());
          manager.send({ cmd: 'list_threads' });
        } catch (e) {
          this.post({ type: 'global_error', message: String(e) });
        }
        return;
      case 'list_commands':
        try {
          await manager.init(resolveWorkspaceCwd());
          manager.send({ cmd: 'list_commands' });
        } catch (e) {
          this.post({ type: 'global_error', message: String(e) });
        }
        return;
      case 'open_thread':
        await this.openThread(msg.sessionId);
        return;
      case 'archive_thread':
        manager.archiveThread(msg.sessionId, msg.archived);
        return;
      case 'pin_thread':
        manager.pinThread(msg.sessionId, msg.pinned);
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
    content="default-src 'none'; img-src ${webview.cspSource} data:; style-src ${webview.cspSource} 'nonce-${nonce}'; script-src 'nonce-${nonce}'; font-src ${webview.cspSource};">
  <link rel="stylesheet" href="${styleUri}">
</head>
<body>
  <div id="root"></div>
  <script nonce="${nonce}" src="${scriptUri}"></script>
</body>
</html>`;
}
