// ChatParticipant handler: drives a dedicated agent session per @manox
// request and projects its events onto the native chat stream. The sidebar
// runs its own session on the same actor; the two never share a turn.

import * as vscode from 'vscode';
import type { FromServer } from '../dist/protocol';
import { notification } from './protocolHelpers';
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
        serverCallSub();
        resolveDone();
      };
      const timer = setTimeout(finish, TURN_TIMEOUT_MS);
      // The participant has no interactive approval surface: authorizations
      // are denied at once with a pointer to the sidebar, where approval
      // cards can be decided interactively. Plans submitted through
      // ProposePlan follow the same rule — the native chat cannot render
      // the review card, so the body streams here and the verdict happens
      // in the sidebar.
      const off = manager.onSessionEvent(sessionId, (ev: Record<string, unknown>) => {
        switch (ev.method) {
          case 'agentText':
            stream.markdown(ev.text as string);
            break;
          case 'agentThinking':
            stream.progress((ev.text as string).trim());
            break;
          case 'toolCall':
            stream.progress(`\u{1F527} ${(ev.title as string) || (ev.name as string)}...`);
            break;
          case 'planReady':
            stream.markdown(`### ${ev.title as string}\n\n${ev.content as string}`);
            stream.markdown(
              `_The plan is awaiting your verdict. Open the **manox sidebar**, select this conversation, and choose Execute / Refine there — plan mode stays on until then._`,
            );
            break;
          case 'planModeChanged':
            stream.markdown(
              ev.enabled
                ? `_Plan mode is on: research is read-only until the submitted plan is approved._`
                : `_Plan mode off._`,
            );
            break;
          case 'error':
            stream.markdown(`**Error:** ${ev.message as string}`);
            finish();
            break;
          case 'turnFinished':
            finish();
            break;
        }
      });
      // Handle ServerCall requests (approve / askUserQuestion / planVerdict).
      const serverCallSub = manager.onSessionServerCall(sessionId, (ev) => {
        if (ev.call.method === 'approve') {
          const call = ev.call as { method: 'approve'; sessionId: string; authId: string; toolName: string; summary: string; input: unknown };
          manager.fromClientReply({
            kind: 'reply',
            id: ev.id,
            outcome: { Ok: { allow: false } },
          });
          stream.markdown(
            `_${call.toolName} requires approval — denied in chat. Use the manox sidebar to approve interactively._`,
          );
        }
      });
      const cancelSub = token.onCancellationRequested(finish);

      try {
        if (token.isCancellationRequested) return { metadata: { cancelled: true } };
        manager.send(notification({
          method: 'submit',
          sessionId,
          text: request.prompt,
          images: [],
          clientId: null,
        }));
        await done;
        return { metadata: { participant: 'manox' } };
      } finally {
        finish();
        off();
        serverCallSub();
        // Disposal cancels an in-flight turn on the server side as well.
        manager.disposeSession(sessionId);
      }
    },
  );
  context.subscriptions.push(participant);
}