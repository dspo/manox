// Out-of-band host protocol. The webview speaks typed `FromClient` /
// `FromServer` (re-exported from `../protocol`) with both hosts; these few
// host-only verbs exist because the VS Code extension intercepts session
// lifecycle (create / open / plan-execute-fresh) to orchestrate its async
// napi surface, while the browser host drives the same lifecycle directly
// via `FromClient`. Neither crosses the agent wire — they are webview↔host
// control messages that the VS Code provider translates into `FromClient`
// sequences against the manager.

import type { FromClient, FromServer, ImageAttachment } from '../protocol';

/** Host-only lifecycle verbs the VS Code extension intercepts. The browser
 * bridge never receives these — the browser webview posts the equivalent
 * `FromClient` sequence instead. */
export type HostVerb =
	| {
			kind: 'new_session';
			sessionId?: string;
			text?: string;
			images?: ImageAttachment[];
			modelId?: string;
	  }
	| { kind: 'open_thread'; sessionId: string }
	| { kind: 'plan_execute_fresh'; sessionId: string; planFile: string; cwd: string };

/** Outbound webview → host: a typed `FromClient`, or a host-only verb. */
export type ToHost = FromClient | HostVerb;

/** Host → webview out-of-band UI note (not from the agent). */
export type HostNote = { kind: 'open_turn_navigator' };

/** Inbound host → webview: a typed `FromServer`, or a host UI note. */
export type ToWebview = FromServer | HostNote;
