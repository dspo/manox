// Protocol helpers: construct FromClient messages for the VS Code host.
// These are defined here rather than in the protocol stub so they work at
// runtime (the protocol.js stub is intentionally empty).

import type { ClientCall, ClientNote, FromClient, MsgId, RpcError } from '../dist/protocol';

/** Events routed per session: everything except the global few. */
export function isSessionEvent(ev: { sessionId?: string | null }): ev is { sessionId: string } {
  return typeof ev.sessionId === 'string';
}

/** Create a FromClient notification message. */
export function notification(note: ClientNote): FromClient {
  return { kind: 'notification', note } as FromClient;
}

/** Create a FromClient request message with a generated id. */
export function request(call: ClientCall): FromClient {
  return { kind: 'request', id: crypto.randomUUID(), call } as unknown as FromClient;
}

/** Create a FromClient reply message. */
export function reply(id: string, outcome: { Ok: unknown } | { Err: RpcError }): FromClient {
  return { kind: 'reply', id: id as MsgId, outcome } as unknown as FromClient;
}