// manox-vscode entry point: registers the @manox ChatParticipant that drives
// the napi agent core (crates/manox-napi).

import * as vscode from 'vscode';
import { registerManoxParticipant } from './participant';

export function activate(context: vscode.ExtensionContext): void {
  registerManoxParticipant(context);
  void vscode.window.showInformationMessage('manox activated — chat with @manox');
}

export function deactivate(): void {}
