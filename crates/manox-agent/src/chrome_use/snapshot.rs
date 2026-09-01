//! Snapshot builder — the accessibility-style page tree with element refs.
//!
//! A walker script runs in the page through the engine's `evaluate` facade:
//! it marks every interesting element with a `data-manox-ref="eN"` attribute
//! (clearing the previous snapshot's marks first) and reports each entry's
//! depth, role, name, and value. Rendering happens here; refs stay valid
//! until the next snapshot or navigation, and actions address elements
//! through the `[data-manox-ref="eN"]` selector. Known limitations:
//! cross-origin iframe content is out of scope, and the walker traverses the
//! light DOM only — shadow-DOM internals (web components) neither get refs
//! nor contribute leaf text.

use std::borrow::Cow;
use std::collections::HashSet;

use rustwright_core::{CancelToken, RustwrightPage};

/// Soft cap for the rendered snapshot text; entries beyond it are elided
/// with a notice rather than flooding the context.
const MAX_TEXT_BYTES: usize = 48 * 1024;
/// Per-name / per-value / per-text display cap (characters).
const DISPLAY_CAP: usize = 160;
/// Indentation depth ceiling so pathological nesting cannot blow up lines.
const MAX_DEPTH: usize = 8;

/// Walker injected via `evaluate`: clears stale refs, marks and reports
/// interesting elements in document order, and collects leaf text. The
/// element set is intentionally compact — actionable controls, headings,
/// images, iframes, and leaf text — so the model sees the page shape without
/// a raw DOM dump.
const WALKER_JS: &str = r###"(() => {
  const REF = "data-manox-ref";
  const MAX = 800;
  const NAME_CAP = 160;
  const out = [];
  let n = 0;

  const INTERACTIVE_TAGS = new Set(["a", "button", "input", "select", "textarea", "summary"]);
  const INTERACTIVE_ROLES = new Set(["button", "link", "checkbox", "radio", "combobox", "listbox",
    "menuitem", "menuitemcheckbox", "menuitemradio", "option", "searchbox", "slider", "spinbutton",
    "switch", "tab", "textbox", "treeitem"]);
  const SKIP_TAGS = new Set(["script", "style", "noscript", "template", "svg", "path", "meta"]);

  const ws = (s) => (s || "").replace(/\s+/g, " ").trim();

  const visible = (el) => {
    const view = el.ownerDocument && el.ownerDocument.defaultView;
    if (!view) return false;
    const st = view.getComputedStyle(el);
    if (st.display === "none" || st.visibility === "hidden") return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 || r.height > 0;
  };

  const roleOf = (el) => {
    const explicit = el.getAttribute && el.getAttribute("role");
    if (explicit && ws(explicit)) return ws(explicit).toLowerCase();
    const tag = el.tagName.toLowerCase();
    if (tag === "a") return el.hasAttribute("href") ? "link" : "";
    if (tag === "button") return "button";
    if (tag === "select") return "combobox";
    if (tag === "textarea") return "textbox";
    if (/^h[1-6]$/.test(tag)) return "heading";
    if (tag === "img") return "img";
    if (tag === "iframe") return "iframe";
    if (tag === "input") {
      const t = (el.type || "text").toLowerCase();
      if (t === "checkbox") return "checkbox";
      if (t === "radio") return "radio";
      if (t === "range") return "slider";
      if (t === "button" || t === "submit" || t === "reset" || t === "image") return "button";
      if (t === "hidden") return "";
      return "textbox";
    }
    return "";
  };

  const labelOf = (el) => {
    const aria = el.getAttribute && el.getAttribute("aria-label");
    if (aria && ws(aria)) return ws(aria);
    const tag = el.tagName;
    if (tag === "IMG") return el.getAttribute("alt") || "";
    if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") {
      if (el.id) {
        try {
          const safe = (window.CSS && window.CSS.escape) ? window.CSS.escape(el.id) : el.id;
          const lab = el.ownerDocument.querySelector("label[for=\"" + safe + "\"]");
          if (lab && ws(lab.textContent)) return ws(lab.textContent);
        } catch (e) {}
      }
      const ph = el.getAttribute && el.getAttribute("placeholder");
      if (ph && ws(ph)) return ws(ph);
      return (el.getAttribute && el.getAttribute("name")) || "";
    }
    return ws(el.textContent);
  };

  const actionable = (el, role) => {
    const tag = el.tagName.toLowerCase();
    if (INTERACTIVE_TAGS.has(tag)) return true;
    if (INTERACTIVE_ROLES.has(role)) return true;
    if (el.isContentEditable) return true;
    if (el.hasAttribute && el.hasAttribute("onclick")) return true;
    const ti = el.getAttribute && el.getAttribute("tabindex");
    if (ti !== null && ti !== undefined && parseInt(ti, 10) >= 0) return true;
    return false;
  };

  document.querySelectorAll("[" + REF + "]").forEach((el) => el.removeAttribute(REF));

  const interesting = (el, role) =>
    actionable(el, role) || role === "heading" || role === "img" || role === "iframe";

  const walk = (node, depth, insideInteresting) => {
    for (const el of node.children) {
      if (out.length >= MAX) return;
      const tag = el.tagName.toLowerCase();
      if (SKIP_TAGS.has(tag)) continue;
      if (!visible(el)) continue;
      const role = roleOf(el);
      if (!insideInteresting && interesting(el, role)) {
        n += 1;
        const ref = "e" + n;
        el.setAttribute(REF, ref);
        const item = { d: depth, r: role || tag, n: ws(labelOf(el)).slice(0, NAME_CAP), ref: ref };
        if (tag === "input" || tag === "textarea" || tag === "select") {
          item.t = (el.type || tag).toLowerCase();
          if (tag !== "select") item.v = item.t === "password" ? null : (el.value || "");
          if (el.checked === true) item.c = true;
        }
        out.push(item);
        // The name already carries the element's text; only containers like
        // <details> or iframes warrant descending.
        if (tag === "details") walk(el, depth + 1, true);
        continue;
      }
      walk(el, depth, insideInteresting);
      if (!insideInteresting && el.childElementCount === 0) {
        const text = ws(el.textContent);
        if (out.length < MAX && text) {
          n += 1;
          const ref = "e" + n;
          el.setAttribute(REF, ref);
          out.push({ d: depth, r: "text", n: text.slice(0, NAME_CAP), ref: ref });
        }
      }
    }
  };

  walk(document.body || document.documentElement, 0, false);
  return { url: location.href, title: document.title, items: out, truncated: out.length >= MAX };
})()"###;

/// Snapshot a page through the walker and render it.
///
/// Returns the rendered tree plus the ref set issued by this snapshot (the
/// caller adopts it as the tab's live set).
pub fn take(
    page: &RustwrightPage,
    cancel: Option<&CancelToken>,
) -> Result<(String, HashSet<String>), String> {
    let wire = page
        .evaluate_with_cancel(
            WALKER_JS,
            None,
            Some(super::runtime::ACTION_TIMEOUT_MS),
            cancel,
        )
        .map_err(|e| format!("snapshot failed: {e}"))?;
    let json = rustwright_core::decode_wire_value(&wire)
        .map_err(|e| format!("snapshot decode failed: {e}"))?;
    let payload: serde_json::Value = serde_json::from_str(&json)
        .map_err(|e| format!("snapshot payload is not valid JSON: {e}"))?;
    render(&payload)
}

/// Render a walker payload into the model-facing tree. Pure — unit-tested
/// without a browser.
pub fn render(payload: &serde_json::Value) -> Result<(String, HashSet<String>), String> {
    let url = payload.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let title = payload.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let items = payload
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "snapshot payload missing `items`".to_string())?;

    let mut refs = HashSet::new();
    let mut out = String::new();
    out.push_str(&format!("- page: {url}"));
    if !title.is_empty() {
        out.push_str(&format!(" — \"{title}\""));
    }
    out.push('\n');

    for (index, item) in items.iter().enumerate() {
        if out.len() > MAX_TEXT_BYTES {
            out.push_str(&format!(
                "... [snapshot truncated: {} more entries elided; scroll or narrow the page and re-snapshot]\n",
                items.len() - index
            ));
            break;
        }
        let depth = item
            .get("d")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(MAX_DEPTH as u64) as usize;
        let indent = "  ".repeat(depth);
        let Some(ref_id) = item.get("ref").and_then(|v| v.as_str()) else {
            continue;
        };
        refs.insert(ref_id.to_string());
        let role = item.get("r").and_then(|v| v.as_str()).unwrap_or("element");
        let name = item.get("n").and_then(|v| v.as_str()).unwrap_or("");
        let mut line = format!("{indent}- {role}");
        if !name.is_empty() {
            line.push_str(&format!(" \"{}\"", truncate(name, DISPLAY_CAP)));
        }
        if let Some(kind) = item.get("t").and_then(|v| v.as_str()) {
            if kind == "password" {
                line.push_str(" value=[masked]");
            } else if let Some(value) = item.get("v").and_then(|v| v.as_str())
                && !value.is_empty()
            {
                line.push_str(&format!(" value=\"{}\"", truncate(value, DISPLAY_CAP)));
            }
        }
        if item.get("c") == Some(&serde_json::Value::Bool(true)) {
            line.push_str(" checked");
        }
        line.push_str(&format!(" [{ref_id}]\n"));
        out.push_str(&line);
    }
    // The walker reports when it hit its own entry cap; surface that instead
    // of letting the model read a seemingly complete tree.
    if payload.get("truncated") == Some(&serde_json::Value::Bool(true)) {
        out.push_str(
            "... [snapshot hit the walker's 800-entry cap; elements beyond it were not \
             captured — narrow the page or scroll and re-snapshot]\n",
        );
    }
    Ok((out, refs))
}

fn truncate(s: &str, max_chars: usize) -> Cow<'_, str> {
    if s.chars().count() <= max_chars {
        return Cow::Borrowed(s);
    }
    let cut: String = s.chars().take(max_chars).collect();
    Cow::Owned(cut + "…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(items: serde_json::Value) -> serde_json::Value {
        json!({ "url": "https://example.test/", "title": "Example", "items": items })
    }

    #[test]
    fn render_builds_tree_and_collects_refs() {
        let (text, refs) = render(&payload(json!([
            { "d": 0, "r": "heading", "n": "Introduction", "ref": "e1" },
            { "d": 0, "r": "text", "n": "Some leaf text.", "ref": "e5" },
            { "d": 1, "r": "textbox", "n": "Search", "ref": "e2", "t": "text", "v": "rust" },
            { "d": 1, "r": "checkbox", "n": "Remember me", "ref": "e3", "t": "checkbox", "c": true },
            { "d": 1, "r": "textbox", "n": "Password", "ref": "e4", "t": "password" },
        ])))
        .unwrap();
        assert!(
            text.starts_with("- page: https://example.test/ — \"Example\"\n"),
            "{text}"
        );
        assert!(text.contains("- heading \"Introduction\" [e1]"), "{text}");
        assert!(text.contains("- text \"Some leaf text.\" [e5]"), "{text}");
        assert!(
            text.contains("  - textbox \"Search\" value=\"rust\" [e2]"),
            "{text}"
        );
        assert!(
            text.contains("  - checkbox \"Remember me\" checked [e3]"),
            "{text}"
        );
        assert!(
            text.contains("  - textbox \"Password\" value=[masked] [e4]"),
            "{text}"
        );
        assert_eq!(
            refs,
            ["e1", "e5", "e2", "e3", "e4"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }
    #[test]
    fn render_masks_password_values() {
        // The walker reports `v: null` for password inputs; even a leaked
        // value must not render.
        let (text, _) = render(&payload(json!([
            { "d": 0, "r": "textbox", "n": "pw", "ref": "e1", "t": "password", "v": "hunter2" },
        ])))
        .unwrap();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("value=[masked]"), "{text}");
    }

    #[test]
    fn render_truncates_oversized_snapshots_with_a_notice() {
        let items: Vec<serde_json::Value> = (0..2000)
            .map(|i| json!({ "d": 0, "r": "text", "n": format!("paragraph text number {i} with some padding words to add bytes"), "ref": format!("e{i}") }))
            .collect();
        let (text, refs) = render(&payload(json!(items))).unwrap();
        assert!(text.len() < MAX_TEXT_BYTES + 4096, "{}", text.len());
        assert!(
            text.contains("[snapshot truncated:"),
            "tail: {}",
            &text[text.len().saturating_sub(120)..]
        );
        assert!(!refs.is_empty());
    }

    #[test]
    fn render_rejects_payload_without_items() {
        let err = render(&json!({ "url": "u" })).unwrap_err();
        assert!(err.contains("items"), "{err}");
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        let s = "中文".repeat(200);
        let cut = truncate(&s, 3);
        assert_eq!(cut, "中文中…");
    }

    #[test]
    fn render_reports_the_walker_entry_cap() {
        let mut payload = payload(json!([]));
        payload["truncated"] = json!(true);
        let (text, refs) = render(&payload).unwrap();
        assert!(text.contains("800-entry cap"), "{text}");
        assert!(refs.is_empty());
    }
}
