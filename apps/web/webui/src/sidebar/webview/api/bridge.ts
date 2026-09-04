// Transport contract the webview renders against: post typed `ToHost`
// messages out, receive typed `FromServer` frames (plus host UI notes) in.
// The VS Code and browser hosts both satisfy this interface — the webview
// never touches a concrete transport, so all business logic stays in the
// app-process side.
//
// T7 v2: the browser transport is connection-observing. `onConnection` lets
// the api client learn about connection generations (reseat the follow
// streams) and drops (flush the pending-request table). The VS Code relay is
// connection-owning (its extension host pumps the napi socket), so it reports
// a single stable generation and never fires either callback afterwards.
import type { FromServer } from '../../../protocol';
import type { HostNote, ToHost } from '../../messages';

export interface Bridge {
	post(message: ToHost): void;
	onMessage(listener: (message: FromServer | HostNote) => void): () => void;
	/** Observe the transport connection. `onConnected` fires once per
	 * established generation (after the Initialize handshake is acked);
	 * `onDisconnected` fires when the link drops. Returns an unsubscribe.
	 * Optional so tests and the VS Code relay can omit real generations. */
	onConnection?(onConnected: () => void, onDisconnected: () => void): () => void;
}

export type { HostNote, ToHost };
