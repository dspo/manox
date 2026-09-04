// manox-vscode entry point: registers the sidebar conversation view and the
// @manox ChatParticipant, both driving the shared napi agent actor
// (crates/manox-napi via manox-actor) through session-scoped channels.

import { randomUUID } from 'node:crypto';
import * as vscode from 'vscode';
import { ManoxModelProvider } from './modelProvider';
import { registerManoxParticipant } from './participant';
import { registerManoxSidebar } from './sidebar/sidebarProvider';
import { SessionManager, configuredApprovalMode } from './sessionManager';

/** §D.2 Initialize identity (T9): minted once and persisted in globalState,
 * replayed on every activation so window reloads and transport re-inits
 * re-seat the same server-side client instead of registering a new one. */
const CLIENT_ID_KEY = 'manox.clientId';

function ensureClientId(context: vscode.ExtensionContext): string {
  const existing = context.globalState.get<string>(CLIENT_ID_KEY);
  if (typeof existing === 'string' && existing.startsWith('vscode-')) return existing;
  const clientId = `vscode-${randomUUID()}`;
  void context.globalState.update(CLIENT_ID_KEY, clientId);
  return clientId;
}

export function activate(context: vscode.ExtensionContext): void {
  // Pin the persisted identity on the shared manager before any surface can
  // trigger the first transport load (the napi Initialize handshake reads
  // it when the process-wide manager is built).
  SessionManager.shared(ensureClientId(context));
  registerManoxSidebar(context);
  registerManoxParticipant(context);
  // The model-provider API is a recent host addition; older hosts keep the
  // sidebar and participant working and simply lack the provider.
  if (typeof vscode.lm.registerLanguageModelChatProvider === 'function') {
    context.subscriptions.push(
      vscode.lm.registerLanguageModelChatProvider(
        'manox',
        new ManoxModelProvider(SessionManager.shared()),
      ),
    );
  }
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('manox.approvalMode')) {
        SessionManager.shared().setApprovalMode(configuredApprovalMode());
      }
    }),
  );
}

export function deactivate(): Thenable<void> {
  return SessionManager.disposeShared();
}
