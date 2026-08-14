// napi bridge: loads crates/manox-napi (manox_napi.node), starts the agent
// actor thread, and fans actor events out to subscribers.

import { EventEmitter } from 'node:events';
import * as path from 'node:path';

interface CoreBinding {
  ping(): string;
  start(callback: (err: null | Error, batch: string[]) => void): void;
  sendCommand(command: string): void;
}

/** Serialized ThreadEvent payload pushed up from the actor thread. */
export interface ActorEvent {
  type: string;
  [key: string]: unknown;
}

let binding: CoreBinding | null = null;
const eventBus = new EventEmitter();

/** Locate and load the napi binding relative to this file (repo or vsix layout). */
export function loadCore(): CoreBinding {
  if (binding) return binding;
  const candidates = [
    path.join(__dirname, '..', 'manox_napi.node'),
    path.join(__dirname, '..', '..', 'target', 'debug', 'manox_napi.node'),
  ];
  let lastErr: unknown = null;
  for (const candidate of candidates) {
    try {
      // eslint-disable-next-line @typescript-eslint/no-var-requires
      const mod = require(candidate) as CoreBinding;
      mod.start((err, batch) => {
        if (err) return;
        for (const raw of batch) {
          try {
            eventBus.emit('event', JSON.parse(raw) as ActorEvent);
          } catch (e) {
            // A subscriber throwing must not drop the rest of the batch.
            console.error('manox event handler error:', e);
          }
        }
      });
      binding = mod;
      return binding;
    } catch (e) {
      lastErr = e;
    }
  }
  throw lastErr instanceof Error ? lastErr : new Error('manox napi core not found');
}

/**
 * Send a command to the agent actor thread. Returns false when the actor is
 * unavailable (e.g. the channel is closed) so callers can surface the failure
 * instead of hanging on a missing turn.
 */
export function sendCommand(cmd: Record<string, unknown>): boolean {
  try {
    loadCore().sendCommand(JSON.stringify(cmd));
    return true;
  } catch (e) {
    console.error('manox sendCommand error:', e);
    return false;
  }
}

/** Subscribe to actor events. Returns an unsubscribe function. */
export function onEvent(handler: (ev: ActorEvent) => void): () => void {
  eventBus.on('event', handler);
  return () => eventBus.off('event', handler);
}
