// ChatParticipant handler: drives the napi agent core for a @manox request,
// projecting actor events onto the native chat stream.

import * as vscode from 'vscode';
import { ensureSession, loadCore, onEvent, sendCommand } from './core';

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
        let cancelSub: vscode.Disposable | undefined;
        const finish = () => {
          if (settled) return;
          settled = true;
          off?.();
          cancelSub?.dispose();
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
        cancelSub = token.onCancellationRequested(finish);
      });

      if (!sendCommand({ cmd: 'submit', text: request.prompt })) {
        stream.markdown('**Error:** agent core unavailable');
        return { metadata: { error: 'core unavailable' } };
      }
      await done;
      return { metadata: { participant: 'manox' } };
    },
  );
  context.subscriptions.push(participant);
}
