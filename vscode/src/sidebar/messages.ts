// postMessage protocol between the sidebar provider (host) and the webview
// renderer. Actor payloads cross the boundary verbatim inside `event`.

import type { ActorEvent, ModelInfo } from '../protocol';

export type WebviewToHost =
  | { type: 'submit'; text: string }
  | { type: 'approve'; id: string; allow: boolean }
  | { type: 'cancel' }
  | { type: 'set_model'; id: string }
  | { type: 'request_models' }
  | { type: 'request_usage' }
  | { type: 'new_session' };

export type HostToWebview =
  | { type: 'session_ready'; sessionId: string; cwd: string }
  | { type: 'session_reset' }
  | { type: 'event'; event: ActorEvent }
  | { type: 'models'; models: ModelInfo[] }
  | { type: 'global_error'; message: string };
