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
  context.subscriptions.push(
    vscode.lm.registerLanguageModelChatProvider(
      'manox',
      new ManoxModelProvider(SessionManager.shared()),
    ),
  );
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
