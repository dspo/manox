// Transport contract the webview renders against: post typed `ToHost`
// messages out, receive typed `ToWebview` payloads in. The VS Code and
// browser hosts both satisfy this interface — the webview never touches a
// concrete transport, so all business logic stays in the app-process side.
import type { ToHost, ToWebview } from '../../messages';

export interface Bridge {
	post(message: ToHost): void;
	onMessage(listener: (message: ToWebview) => void): () => void;
}
