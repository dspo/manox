// ChatParticipant handler: drives a dedicated agent session per @manox
// request and projects its events onto the native chat stream. The sidebar
// runs its own session on the same actor; the two never share a turn.

import * as vscode from 'vscode';
import type { ActorEvent } from './protocol';
import { SessionManager, resolveWorkspaceCwd } from './sessionManager';

const TURN_TIMEOUT_MS = 120_000;

export function registerManoxParticipant(context: vscode.ExtensionContext): void {
  const participant = vscode.chat.createChatParticipant(
    'manox',
    async (request, _ctx, stream, token) => {
      const manager = SessionManager.shared();
      const cwd = resolveWorkspaceCwd();

      let sessionId: string;
      try {
        await manager.init(cwd);
        sessionId = await manager.createSession(cwd);
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        stream.markdown(`**Error:** manox core unavailable (${msg})`);
        return { metadata: { error: 'core unavailable' } };
      }

      let settled = false;
      let resolveDone!: () => void;
      const done = new Promise<void>((resolve) => (resolveDone = resolve));
      const finish = () => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        cancelSub.dispose();
        resolveDone();
      };
      const timer = setTimeout(finish, TURN_TIMEOUT_MS);
      // The participant has no interactive approval surface: authorizations
      // are denied at once with a pointer to the sidebar, where approval
      // cards can be decided interactively.
      const off = manager.onSessionEvent(sessionId, (ev: ActorEvent) => {
        switch (ev.type) {
          case 'agent_text':
            stream.markdown(ev.text);
            break;
          case 'agent_thinking':
            stream.progress(ev.text.trim());
            break;
          case 'tool_call':
            stream.progress(`🔧 ${ev.title || ev.name}…`);
            break;
          case 'tool_call_authorization':
            manager.send({ cmd: 'approve', sessionId, id: ev.id, allow: false });
            stream.markdown(
              `_${ev.tool_name} requires approval — denied in chat. Use the manox sidebar to approve interactively._`,
            );
            break;
          case 'error':
            stream.markdown(`**Error:** ${ev.message}`);
            finish();
            break;
          case 'turn_finished':
            finish();
            break;
        }
      });
      const cancelSub = token.onCancellationRequested(finish);

      try {
        if (token.isCancellationRequested) return { metadata: { cancelled: true } };
        manager.send({ cmd: 'submit', sessionId, text: request.prompt });
        await done;
        return { metadata: { participant: 'manox' } };
      } finally {
        finish();
        off();
        // Disposal cancels an in-flight turn on the actor side as well.
        manager.disposeSession(sessionId);
      }
    },
  );
  context.subscriptions.push(participant);
}
