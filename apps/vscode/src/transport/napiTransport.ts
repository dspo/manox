// Transport backed by the manox napi native binding: the agent server runs
// in-process on its own tokio runtime, and FromServer messages arrive through
// a napi threadsafe function in Node callback style `(err, eventJson)`.
//
// T9: the pump (Rust) forwards every `FromServer` variant verbatim — v2
// stream/host frames included — and `sendCommand` accepts every `FromClient`
// variant. The JSON.parse below is the host's only frame inspection: frames
// are never filtered by kind (unknown-future-tolerant: serde on the Rust
// side is a closed enum, so a `FromClient` this host's protocol crate cannot
// decode rejects here; callers log and drop rather than crash).

import { EventEmitter } from 'node:events';
import * as fs from 'node:fs';
import * as path from 'node:path';
import type { FromServer } from '../../dist/protocol';
import type { Transport } from './transport';

/** The full v2 `FromServer` envelope vocabulary (§D). The pump forwards
 * every variant verbatim; this is the relay's only frame inspection — a
 * parsed object whose `kind` is not one of these is a frame this host's
 * bindings cannot describe, and it is logged + dropped (never fatal). */
const FROM_SERVER_KINDS = new Set<string>([
  'response',
  'request',
  'notification',
  'host',
  'streamItem',
  'streamEnd',
]);

function isFromServer(value: unknown): value is FromServer {
  return (
    typeof value === 'object' &&
    value !== null &&
    typeof (value as { kind?: unknown }).kind === 'string' &&
    FROM_SERVER_KINDS.has((value as { kind: string }).kind)
  );
}

interface NapiBinding {
  ping(): string;
  start(clientId: string, callback: (err: Error | null, event: string) => void): void;
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

  /** Load the binding and start the agent server connection. `clientId` is
   * the persisted host identity (§D.2 Initialize) the extension mints once
   * (globalState) and replays on every re-init so the server re-seats the
   * same client; an empty string falls back to the legacy `"vscode"` pin. */
  static load(clientId = ''): NapiTransport {
    const transport = new NapiTransport(loadBinding());
    transport.binding.start(clientId, (err, raw) => {
      if (err) {
        console.error('manox transport error:', err);
        return;
      }
      try {
        const msg = JSON.parse(raw) as unknown;
        if (!isFromServer(msg)) {
          // Unknown envelope shape (version skew between the bundled webview
          // and this host's protocol crate): log + drop, never disconnect.
          console.error('manox: dropping unknown FromServer frame:', raw);
          return;
        }
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