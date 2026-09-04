// Sidebar host: a transparent typed relay between the webview renderer and
// the shared agent server (T9: the v2 envelope is relayed verbatim in both
// directions). The webview speaks `FromClient` / `FromServer` directly; the
// host forwards `FromClient` — `StreamOpen` / `StreamCancel` included — to
// the napi `AgentServer` connection, and relays EVERY raw `FromServer` frame
// (notification / request / response / host / streamItem / streamEnd) back
// to the webview unfiltered: the shared v2 bundle in the webview owns frame
// interpretation and answers `request` frames itself (the host never
// intercepts them). Only the few host-only lifecycle verbs (`HostVerb`) are
// intercepted, because the VS Code `SessionManager` owns the napi connection
// and session lifecycle — the browser host has no such orchestrator and
// posts the equivalent `FromClient` sequence itself.

import * as crypto from 'node:crypto';
import * as vscode from 'vscode';
import type { FromClient, FromServer, ImageAttachment } from '../../dist/protocol';
import { notification, request } from '../protocolHelpers';
import { SessionManager, resolveWorkspaceCwd } from '../sessionManager';
import type { HostVerb, ToHost, ToWebview } from '../../dist/sidebar/messages';

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
	/** Live session ids, for teardown disposal on the actor side. View
	 * switching never removes entries — only teardown does. Frame delivery
	 * needs no per-session subscription: the manager's raw `onFrame` relay
	 * forwards every session's frames verbatim (T9), and the webview's v2
	 * store routes them by `streamId` / `sessionId`. */
	private readonly sessions = new Set<string>();
	private unsubscribeFrames: (() => void) | null = null;
	/** Monotonic token invalidated by teardown: an in-flight create/open
	 * completing afterwards disposes its actor-side session instead of
	 * attaching to a dead view. */
	private sessionGeneration = 0;

	constructor(private readonly extensionUri: vscode.Uri) {}

	/** Toggle the turn navigator from the host (macOS cmd+m path). */
	openTurnNavigator(): void {
		this.post({ kind: 'open_turn_navigator' });
	}

	resolveWebviewView(webviewView: vscode.WebviewView): void {
		this.view = webviewView;
		webviewView.webview.options = {
			enableScripts: true,
			localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, 'dist')],
		};
		webviewView.webview.html = renderHtml(webviewView.webview, this.extensionUri);

		webviewView.webview.onDidReceiveMessage((msg: ToHost) => {
			void this.onWebviewMessage(msg);
		});
		webviewView.onDidDispose(() => this.teardown());

		// T9 raw relay: forward EVERY `FromServer` frame verbatim — legacy
		// notification / response arms plus the v2 `streamItem` / `streamEnd`
		// / `host` arms. The webview bundle interprets frames and answers
		// `request` (ServerCall) frames itself; the host never filters or
		// re-wraps anything.
		this.unsubscribeFrames = SessionManager.shared().onFrame((msg: FromServer) =>
			this.post(msg),
		);

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
			this.registerSession(sessionId);
			if (opts?.modelId) {
				manager.send(notification({ method: 'setModel', sessionId, id: opts.modelId }));
			}
			if (opts?.text || opts?.images?.length) {
				manager.send(
					notification({
						method: 'submit',
						sessionId,
						text: opts.text ?? '',
						images: opts.images ?? [],
						clientId: null,
					}),
				);
			}
		} catch (e) {
			this.postError(e);
		}
	}

	private async initActor(): Promise<void> {
		try {
			await SessionManager.shared().init(resolveWorkspaceCwd());
		} catch (e) {
			this.postError(e);
		}
	}

	private async openThread(sessionId: string): Promise<void> {
		const manager = SessionManager.shared();
		if (this.sessions.has(sessionId)) {
			// Already live: re-announce so a reloaded webview rebuilds from
			// scratch, then have the server replay history/info via a fresh
			// `OpenSession`.
			this.post({
				kind: 'notification',
				note: { method: 'sessionCreated', sessionId },
			});
			manager.send(notification({ method: 'focusThread', sessionId: null }));
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
			this.registerSession(sessionId);
		} catch (e) {
			this.postError(e);
		}
	}

	/** Track a confirmed session for teardown disposal. No per-session event
	 * subscription is needed since T9: the raw `onFrame` relay already
	 * forwards the session's frames (`sessionCreated`, journal streams,
	 * ServerCall requests) verbatim to the webview, and the v2 store there
	 * handles readiness and replies. */
	private registerSession(sessionId: string): void {
		this.sessions.add(sessionId);
	}

	private async onWebviewMessage(msg: ToHost): Promise<void> {
		const manager = SessionManager.shared();
		// Host-only lifecycle verbs: the VS Code SessionManager owns the napi
		// connection and per-session routing, so it orchestrates these.
		switch (msg.kind) {
			case 'new_session':
				await this.newSession({
					sessionId: msg.sessionId,
					text: msg.text,
					images: msg.images,
					modelId: msg.modelId,
				});
				return;
			case 'open_thread':
				await this.openThread(msg.sessionId);
				return;
			case 'plan_execute_fresh': {
				void (async () => {
					const generation = ++this.sessionGeneration;
					try {
						await manager.init(resolveWorkspaceCwd());
						manager.send(
							notification({
								method: 'archiveThread',
								sessionId: msg.sessionId,
								archived: true,
							}),
						);
						const freshId = crypto.randomUUID();
						await manager.createSession(msg.cwd, freshId);
						if (generation !== this.sessionGeneration) {
							manager.disposeSession(freshId);
							return;
						}
						this.registerSession(freshId);
						manager.send(
							notification({
								method: 'planSeedExecution',
								sessionId: freshId,
								planFile: msg.planFile,
							}),
						);
					} catch (e) {
						this.postError(e);
					}
				})();
				return;
			}
		}
		// Otherwise it is a typed `FromClient` (notification / request /
		// reply / streamOpen / streamCancel): forward verbatim to the agent
		// server — the napi side accepts the full v2 envelope.
		manager.send(msg as FromClient);
	}

	private postError(e: unknown): void {
		this.post({
			kind: 'notification',
			note: {
				method: 'error',
				sessionId: null,
				message: `manox core unavailable: ${e instanceof Error ? e.message : String(e)}`,
			},
		});
	}

	private teardown(): void {
		this.sessionGeneration++;
		const manager = SessionManager.shared();
		for (const sessionId of this.sessions) manager.disposeSession(sessionId);
		this.sessions.clear();
		this.unsubscribeFrames?.();
		this.unsubscribeFrames = null;
		this.view = null;
	}

	private post(message: ToWebview): void {
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
