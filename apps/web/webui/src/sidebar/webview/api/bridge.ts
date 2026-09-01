// Transport contract the webview renders against: post typed messages out,
// receive typed HostToWebview payloads in. The VS Code and browser hosts
// both satisfy this interface — the webview never touches a concrete
// transport, so all business logic stays in the app-process Rust side.
import type { HostToWebview, WebviewToHost } from '../../messages';

export interface Bridge {
  post(message: WebviewToHost): void;
  onMessage(listener: (message: HostToWebview) => void): () => void;
}
