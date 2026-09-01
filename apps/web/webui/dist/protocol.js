"use strict";
// Wire protocol between the TypeScript host and the manox actor. Single
// source of truth; the Rust side mirrors it in
// crates/manox-actor/src/{actor,events}.rs (exposed via manox-napi).
//
// Every event carries `sessionId` except the global ones (`ready`, `models`,
// `threads_updated`, `commands`, `model_*`, and errors raised before a
// session exists).
// Every command carries `sessionId` except `init`, `list_models`,
// `list_threads`, `list_commands`, `model_chat`, and `cancel_model_chat`.
// Actor shutdown does not go over this
// protocol — the napi binding terminates the thread directly.
Object.defineProperty(exports, "__esModule", { value: true });
exports.isSessionEvent = isSessionEvent;
/** Events routed per session; everything except the global few. */
function isSessionEvent(ev) {
    return typeof ev.sessionId === 'string';
}
