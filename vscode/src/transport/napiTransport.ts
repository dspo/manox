// Transport backed by the manox-napi native binding: the agent server runs
// in-process on its own tokio runtime, and FromServer messages arrive through
// a napi threadsafe function in Node callback style `(err, eventJson)`.

import { EventEmitter } from 'node:events';
import * as fs from 'node:fs';
import * as path from 'node:path';
import type { FromServer } from '../../dist/protocol';
import type { Transport } from './transport';

interface NapiBinding {
  ping(): string;
  start(callback: (err: Error | null, event: string) => void): void;
  sendCommand(command: string): void;
  shutdown(): void;
}

/**
 * Locate the binding. `__dirname` is `<extensionRoot>/out/transport` in the
 * compiled host, so the packaged binding (extension root) is two levels up;
 * a repo `target/` build covers extension-development hosts run straight
 * from the workspace.
 */
function loadBinding(): NapiBinding {
  const candidates = [
    path.join(__dirname, '..', '..', 'manox_napi.node'),
    path.join(__dirname, '..', 'manox_napi.node'),
    path.join(__dirname, '..', '..', '..', 'target', 'debug', 'manox_napi.node'),
  ];
  let loadErr: unknown = null;
  for (const candidate of candidates) {
    if (!fs.existsSync(candidate)) continue;
    try {
      // eslint-disable-next-line @typescript-eslint/no-var-requires
      return require(candidate) as NapiBinding;
    } catch (e) {
      loadErr = e;
    }
  }
  const detail = loadErr instanceof Error ? loadErr.message : 'no candidate found';
  throw new Error(`manox napi binding unavailable (${detail})`);
}

export class NapiTransport implements Transport {
  private readonly events = new EventEmitter();
  private readonly readyPromise: Promise<void>;
  private disposed = false;

  private constructor(private readonly binding: NapiBinding) {
    this.events.setMaxListeners(0);
    this.readyPromise = Promise.resolve();
  }

  /** Load the binding and start the agent server connection. */
  static load(): NapiTransport {
    const transport = new NapiTransport(loadBinding());
    transport.binding.start((err, raw) => {
      if (err) {
        console.error('manox transport error:', err);
        return;
      }
      try {
        const msg = JSON.parse(raw) as FromServer;
        transport.events.emit('event', msg);
      } catch (e) {
        console.error('manox: malformed FromServer event:', raw, e);
      }
    });
    return transport;
  }

  get ready(): Promise<void> {
    return this.readyPromise;
  }

  onEvent(handler: (ev: FromServer) => void): () => void {
    this.events.on('event', handler);
    return () => this.events.off('event', handler);
  }

  send(command: string): void {
    if (this.disposed) throw new Error('manox transport is disposed');
    this.binding.sendCommand(command);
  }

  async dispose(): Promise<void> {
    if (this.disposed) return;
    this.disposed = true;
    this.binding.shutdown();
    this.events.removeAllListeners();
  }
}