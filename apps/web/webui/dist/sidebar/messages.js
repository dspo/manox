"use strict";
// postMessage protocol between the sidebar provider (host) and the webview
// renderer. Actor payloads cross the boundary verbatim inside `event`,
// except the global snapshots unwrapped into their own messages. Per-thread
// messages always carry their sessionId — view switching is pure webview
// state, so the host routes by id and never infers a "current" session.
Object.defineProperty(exports, "__esModule", { value: true });
