// Inbound write request bridge — independent of notify.js.
//
// A page calls __manox_request_write__ when it wants manox to perform an
// internal write on its behalf. This NEVER executes directly: Rust routes the
// request through a mandatory confirmation overlay that does NOT read
// ApprovalMode — the inbound trust axis is orthogonal to the outbound one,
// so a Yolo session cannot let a web page invoke Rust write surfaces (which
// would be a privilege-escalation channel).
//
// `intent` is a closed command name registered by manox (currently none —
// the architecture is in place for future write surfaces). Unknown intents
// are rejected by the confirmation overlay.
(function () {
  function post(body) {
    try {
      fetch("manox://request", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      }).catch(function () {
        // fire-and-forget.
      });
    } catch (_) {
      // fetch unavailable or scheme blocked — drop silently.
    }
  }

  window.__manox_request_write__ = function (intent, payload) {
    post({ intent: intent, payload: payload === undefined ? null : payload });
  };
})();
