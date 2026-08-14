// ChatParticipant handler: drives the napi agent core for a @manox request,
// projecting actor events onto the native chat stream.

import * as vscode from 'vscode';
import { loadCore, onEvent, sendCommand, type ActorEvent } from './core';

let initialized = false;

function ensureSession(cwd: string): void {
  if (initialized) return;
  sendCommand({ cmd: 'init', cwd });
  sendCommand({ cmd: 'create_session', cwd });
  initialized = true;
}

function resolveCwd(): string {
  const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  return folder ?? process.env.HOME ?? process.cwd();
}

export function registerManoxParticipant(context: vscode.ExtensionContext): void {
  const participant = vscode.chat.createChatParticipant(
    'manox',
    async (request, _ctx, stream, token) => {
      loadCore();
      ensureSession(resolveCwd());

      // Await turn completion (stop/turn_finished), cancellation, or a safety
      // timeout — whichever comes first, then tear down the subscription.
      const done = new Promise<void>((resolve) => {
        let settled = false;
        let off: (() => void) | undefined;
        const finish = () => {
          if (settled) return;
          settled = true;
          off?.();
          clearTimeout(timer);
          resolve();
        };
        off = onEvent((ev) => {
          switch (ev.type) {
            case 'agent_text':
              stream.markdown(ev.text as string);
              break;
            case 'agent_thinking':
              stream.progress((ev.text as string).trim());
              break;
            case 'tool_call':
              stream.progress(`🔧 ${ev.name as string}…`);
              break;
            case 'error':
              stream.markdown(`**Error:** ${ev.message as string}`);
              finish();
              break;
            case 'stop':
            case 'turn_finished':
              finish();
              break;
          }
        });
        const timer = setTimeout(finish, 120_000);
        token.onCancellationRequested(finish);
      });

      sendCommand({ cmd: 'submit', text: request.prompt });
      await done;
      return { metadata: { participant: 'manox' } };
    },
  );
  context.subscriptions.push(participant);
}
