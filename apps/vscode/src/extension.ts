// manox-vscode entry point: registers the sidebar conversation view and the
// @manox ChatParticipant, both driving the shared napi agent actor
// (crates/manox-napi via manox-actor) through session-scoped channels.

import * as vscode from 'vscode';
import { ManoxModelProvider } from './modelProvider';
import { registerManoxParticipant } from './participant';
import { registerManoxSidebar } from './sidebar/sidebarProvider';
import { SessionManager, configuredApprovalMode } from './sessionManager';

export function activate(context: vscode.ExtensionContext): void {
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
