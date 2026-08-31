// Transport abstraction between the extension host and the manox agent
// server. The protocol lives in ../dist/protocol; a transport moves typed
// FromServer messages, one per delivery. The napi binding is the current
// implementation; a stdio child process can implement the same interface.

import type { FromServer } from '../../dist/protocol';

export interface Transport {
  /** Resolves once the actor is up and commands may be sent. */
  readonly ready: Promise<void>;
  /** Subscribe to server events; returns an unsubscribe function. */
  onEvent(handler: (ev: FromServer) => void): () => void;
  /** Deliver one serialized FromClient JSON string. Throws when the actor is unreachable. */
  send(command: string): void;
  /** Shut the actor down and release its resources. */
  dispose(): Promise<void>;
}