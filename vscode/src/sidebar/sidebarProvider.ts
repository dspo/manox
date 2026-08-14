// Sidebar host: owns the sidebar's agent session and bridges it to the
// webview renderer. The webview is a pure renderer — all actor access goes
// through this provider, translating the postMessage protocol (./messages)
// into session commands.

import * as crypto from 'node:crypto';
import * as vscode from 'vscode';
import type { ActorEvent } from '../protocol';
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
  private sessionId: string | null = null;
  private unsubscribeSession: (() => void) | null = null;
  private unsubscribeGlobal: (() => void) | null = null;

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
      if (ev.type === 'error') this.post({ type: 'global_error', message: ev.message });
    });

    void this.ensureSession();
  }

  /** Replace the sidebar session (manox.newSession command). */
  async newSession(): Promise<void> {
    this.teardownSession();
    this.post({ type: 'session_reset' });
    await this.ensureSession();
  }

  private async ensureSession(): Promise<void> {
    const manager = SessionManager.shared();
    const cwd = resolveWorkspaceCwd();
    try {
      await manager.init(cwd);
      const sessionId = await manager.createSession(cwd);
      this.sessionId = sessionId;
      this.unsubscribeSession = manager.onSessionEvent(sessionId, (ev: ActorEvent) =>
        this.post({ type: 'event', event: ev }),
      );
      this.post({ type: 'session_ready', sessionId, cwd });
      manager.send({ cmd: 'get_current_model', sessionId });
      void manager.listModels().then(
        (models) => this.post({ type: 'models', models }),
        (e: unknown) => this.post({ type: 'global_error', message: String(e) }),
      );
    } catch (e) {
      this.post({
        type: 'global_error',
        message: `manox core unavailable: ${e instanceof Error ? e.message : String(e)}`,
      });
    }
  }

  private async onWebviewMessage(msg: WebviewToHost): Promise<void> {
    const manager = SessionManager.shared();
    const sessionId = this.sessionId;
    switch (msg.type) {
      case 'submit':
        if (sessionId) manager.send({ cmd: 'submit', sessionId, text: msg.text });
        return;
      case 'approve':
        if (sessionId) manager.send({ cmd: 'approve', sessionId, id: msg.id, allow: msg.allow });
        return;
      case 'cancel':
        if (sessionId) manager.send({ cmd: 'cancel_turn', sessionId });
        return;
      case 'set_model':
        if (sessionId) manager.send({ cmd: 'set_model', sessionId, id: msg.id });
        return;
      case 'request_usage':
        if (sessionId) manager.send({ cmd: 'get_usage', sessionId });
        return;
      case 'request_models':
        try {
          this.post({ type: 'models', models: await manager.listModels() });
        } catch (e) {
          this.post({ type: 'global_error', message: String(e) });
        }
        return;
      case 'new_session':
        await this.newSession();
        return;
    }
  }

  private teardown(): void {
    this.teardownSession();
    this.unsubscribeGlobal?.();
    this.unsubscribeGlobal = null;
    this.view = null;
  }

  private teardownSession(): void {
    this.unsubscribeSession?.();
    this.unsubscribeSession = null;
    if (this.sessionId) {
      SessionManager.shared().disposeSession(this.sessionId);
      this.sessionId = null;
    }
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
