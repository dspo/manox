// Transport abstraction between the extension host and the manox agent
// actor. The wire protocol lives in ../protocol; a transport moves raw JSON
// strings, one event per delivery. The napi binding is the current
// implementation; a stdio child process can implement the same interface.

import type { ActorEvent } from '../../dist/protocol';

export interface Transport {
  /** Resolves once the actor is up and commands may be sent. */
  readonly ready: Promise<void>;
  /** Subscribe to actor events; returns an unsubscribe function. */
  onEvent(handler: (ev: ActorEvent) => void): () => void;
  /** Deliver one serialized command. Throws when the actor is unreachable. */
  send(command: string): void;
  /** Shut the actor down and release its resources. */
  dispose(): Promise<void>;
}
