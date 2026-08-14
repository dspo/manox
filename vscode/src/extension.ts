// manox-vscode entry point: registers the @manox ChatParticipant that drives
// the napi agent core (crates/manox-napi).

import * as vscode from 'vscode';
import { registerManoxParticipant } from './participant';
import { registerManoxView } from './view';

export function activate(context: vscode.ExtensionContext): void {
  registerManoxView(context);
  registerManoxParticipant(context);
  console.log('manox-vscode activated');
}

export function deactivate(): void {}
