// Minimal, trust-bound bridge from an untrusted page back to Rust.
//
// Untrusted webviews get ONLY this bridge injected (no __TAURI_INTERNALS__,
// no plugin command surface). A page can tell manox "something happened" via
// __manox_notify__, but it cannot ask manox to execute anything — the notify
// type is a closed enum parsed on the Rust side, never a command name.
//
// `type` is one of: "page_loaded" | "dom_changed" | "navigation" |
// "user_handback" | "eval_result". Anything else is dropped on the Rust side.
// `payload` shape depends on `type`; see `parse_notification` in lib.rs.
(function () {
  function post(body) {
    try {
      fetch("manox://notify", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      }).catch(function () {
        // fire-and-forget; the page never learns whether Rust accepted.
      });
    } catch (_) {
      // fetch unavailable or scheme blocked — drop silently.
    }
  }

  window.__manox_notify__ = function (type, payload) {
    post({ type: type, payload: payload === undefined ? null : payload });
  };
})();
