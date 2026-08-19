//! ChromeUse session runtime — the process-wide Chrome driven by the
//! in-process rustwright CDP engine.
//!
//! The singleton owns the browser handle and the tab table. Every method is
//! synchronous (the engine facade blocks on the engine's own runtime), so
//! tool executions reach it through `super::bridge::run`, never from an
//! async context. The session mutex serializes all engine access, keeping
//! the shared browser coherent when several threads drive it at once.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use rustwright_core::{CancelToken, RustwrightBrowser, RustwrightPage};

use crate::settings::ChromeSettings;

/// Element-action budget for engine calls, in milliseconds.
pub const ACTION_TIMEOUT_MS: f64 = 30_000.0;
/// Navigation budget for engine calls, in milliseconds.
pub const NAVIGATION_TIMEOUT_MS: f64 = 60_000.0;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const PAGES_TIMEOUT: Duration = Duration::from_secs(30);
/// Text-presence polling budget for `ChromeUseWaitFor`.
const WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_POLL: Duration = Duration::from_millis(500);

/// Process-unique handle for a ChromeUse tab, issued by the runtime.
pub type ChromeTabId = u64;

/// One tracked tab: the engine page handle plus the ref set issued by the
/// tab's latest snapshot. Actions validate ref membership so a stale ref
/// fails fast with an actionable message instead of hitting the DOM.
pub struct TabEntry {
    pub page: RustwrightPage,
    pub refs: HashSet<String>,
    target_id: String,
}

struct Session {
    browser: RustwrightBrowser,
    next_tab_id: ChromeTabId,
    tabs: HashMap<ChromeTabId, TabEntry>,
}

pub struct ChromeUseRuntime {
    session: Mutex<Option<Session>>,
}

static RUNTIME: OnceLock<ChromeUseRuntime> = OnceLock::new();

/// The process-wide ChromeUse runtime.
pub fn runtime() -> &'static ChromeUseRuntime {
    RUNTIME.get_or_init(|| ChromeUseRuntime {
        session: Mutex::new(None),
    })
}

/// Close any live session at app exit; an owned Chrome process goes with it.
pub fn shutdown() {
    if let Some(rt) = RUNTIME.get() {
        rt.close_session_quiet();
    }
}

/// Wire JSON for the engine's launch facade. `headless` is always explicit —
/// the engine's own default is headless, the opposite of the ChromeUse
/// default (a user-visible Chrome). `DISABLE_TELEMETRY` rides the launched
/// process's env so the engine never phones home.
fn launch_options(chrome: &ChromeSettings, headless: Option<bool>) -> String {
    let user_data_dir = chrome
        .user_data_dir
        .clone()
        .unwrap_or_else(default_profile_dir);
    serde_json::json!({
        "headless": headless.unwrap_or(chrome.headless),
        "executable_path": chrome.executable,
        "channel": null,
        "args": [],
        "ignore_all_default_args": false,
        "ignore_default_args": [],
        "user_data_dir": user_data_dir,
        "env": { "DISABLE_TELEMETRY": "1" },
        "chromium_sandbox": false,
        "proxy": null,
    })
    .to_string()
}

fn default_profile_dir() -> String {
    crate::paths::manox_config_dir()
        .map(|dir| dir.join("chrome-profile"))
        .unwrap_or_else(|_| std::env::temp_dir().join("manox-chrome-profile"))
        .to_string_lossy()
        .into_owned()
}

impl ChromeUseRuntime {
    fn lock(&self) -> MutexGuard<'_, Option<Session>> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn session_mut(guard: &mut Option<Session>) -> Result<&mut Session, String> {
        guard
            .as_mut()
            .ok_or_else(|| "no Chrome session is open; call ChromeUseOpen first".into())
    }

    /// Ensure a live session exists, launching or attaching on first use.
    /// Launch/attach options only apply when the session starts; an existing
    /// session is reused as-is.
    pub fn ensure_session(
        &self,
        cdp_endpoint: Option<&str>,
        headless: Option<bool>,
        cancel: Option<&CancelToken>,
    ) -> Result<(), String> {
        let mut guard = self.lock();
        if let Some(session) = guard.as_ref() {
            if session.browser.is_connected() {
                return Ok(());
            }
            // The connection died (Chrome closed externally): rebuild below.
            *guard = None;
        }
        let chrome = crate::settings::load().chrome;
        let endpoint = cdp_endpoint
            .map(str::to_string)
            .or(chrome.cdp_endpoint.clone());
        let browser = match endpoint {
            Some(endpoint) => rustwright_core::rustwright_connect_over_cdp_with_cancel(
                &endpoint,
                &[],
                CONNECT_TIMEOUT,
                cancel,
            )
            .map_err(|e| format!("failed to attach to Chrome at {endpoint}: {e}"))?,
            None => {
                let options = launch_options(&chrome, headless);
                rustwright_core::rustwright_launch_chromium_with_cancel(&options, cancel).map_err(
                    |e| {
                        format!(
                            "failed to launch Chrome: {e}. If Chrome lives in a custom \
                             location, set `[chrome].executable` in settings.toml or \
                             RUSTWRIGHT_CHROMIUM."
                        )
                    },
                )?
            }
        };
        *guard = Some(Session {
            browser,
            next_tab_id: 1,
            tabs: HashMap::new(),
        });
        Ok(())
    }

    /// Open a fresh tab, optionally navigated to `url`.
    pub fn open_tab(
        &self,
        url: Option<&str>,
        cancel: Option<&CancelToken>,
    ) -> Result<ChromeTabId, String> {
        let mut guard = self.lock();
        let session = Self::session_mut(&mut guard)?;
        let page = session
            .browser
            .new_page_with_cancel(cancel)
            .map_err(|e| format!("failed to open a new tab: {e}"))?;
        if let Some(url) = url
            && let Err(e) =
                page.goto_with_cancel(url, None, Some(NAVIGATION_TIMEOUT_MS), None, cancel)
        {
            let _ = page.close(Some(ACTION_TIMEOUT_MS), false);
            return Err(format!("navigation to {url} failed: {e}"));
        }
        let id = session.next_tab_id;
        session.next_tab_id += 1;
        session.tabs.insert(
            id,
            TabEntry {
                target_id: page.target_id(),
                page,
                refs: HashSet::new(),
            },
        );
        Ok(id)
    }

    /// Run `f` against a tracked tab's entry under the session lock.
    pub fn with_tab<F, R>(&self, tab_id: ChromeTabId, f: F) -> Result<R, String>
    where
        F: FnOnce(&mut TabEntry) -> Result<R, String>,
    {
        let mut guard = self.lock();
        let session = Self::session_mut(&mut guard)?;
        let entry = session
            .tabs
            .get_mut(&tab_id)
            .ok_or_else(|| format!("unknown tab_id {tab_id}; list open tabs with ChromeUseTabs"))?;
        f(entry)
    }

    /// Translate a live snapshot ref into the CSS selector addressing it.
    /// Membership in the latest snapshot implies the `eN` shape, so a
    /// selector built from a live ref is injection-safe.
    pub fn ref_selector(&self, tab_id: ChromeTabId, ref_id: &str) -> Result<String, String> {
        let mut guard = self.lock();
        let session = Self::session_mut(&mut guard)?;
        let entry = session
            .tabs
            .get(&tab_id)
            .ok_or_else(|| format!("unknown tab_id {tab_id}; list open tabs with ChromeUseTabs"))?;
        if entry.refs.contains(ref_id) {
            Ok(format!("[data-manox-ref=\"{ref_id}\"]"))
        } else {
            Err(format!(
                "stale or unknown ref `{ref_id}` on tab {tab_id}; take a fresh ChromeUseSnapshot"
            ))
        }
    }

    /// Take a snapshot and adopt its ref set as the tab's live set.
    pub fn snapshot(
        &self,
        tab_id: ChromeTabId,
        cancel: Option<&CancelToken>,
    ) -> Result<String, String> {
        self.with_tab(tab_id, |entry| {
            let (text, refs) = super::snapshot::take(&entry.page, cancel)?;
            entry.refs = refs;
            Ok(text)
        })
    }

    /// Navigate a tab and return the post-navigation snapshot.
    pub fn navigate(
        &self,
        tab_id: ChromeTabId,
        url: &str,
        cancel: Option<&CancelToken>,
    ) -> Result<String, String> {
        self.with_tab(tab_id, |entry| {
            entry
                .page
                .goto_with_cancel(url, None, Some(NAVIGATION_TIMEOUT_MS), None, cancel)
                .map_err(|e| format!("navigation to {url} failed: {e}"))?;
            entry.refs.clear();
            let (text, refs) = super::snapshot::take(&entry.page, cancel)?;
            entry.refs = refs;
            Ok(text)
        })
    }

    /// Evaluate JavaScript in a tab (Playwright semantics) and return the
    /// decoded JSON wire representation of the result.
    pub fn evaluate(
        &self,
        tab_id: ChromeTabId,
        function: &str,
        cancel: Option<&CancelToken>,
    ) -> Result<String, String> {
        self.with_tab(tab_id, |entry| {
            let wire = entry
                .page
                .evaluate_with_cancel(function, None, Some(ACTION_TIMEOUT_MS), cancel)
                .map_err(|e| format!("evaluate failed: {e}"))?;
            rustwright_core::decode_wire_value(&wire)
                .map_err(|e| format!("evaluate result decode failed: {e}"))
        })
    }

    /// Capture a PNG screenshot; returns the bytes and the tab's current url.
    pub fn screenshot(
        &self,
        tab_id: ChromeTabId,
        full_page: bool,
        cancel: Option<&CancelToken>,
    ) -> Result<(Vec<u8>, String), String> {
        self.with_tab(tab_id, |entry| {
            let url = entry.page.url();
            let bytes = entry
                .page
                .screenshot_with_cancel(
                    None,
                    Some(full_page),
                    None,
                    Some(ACTION_TIMEOUT_MS),
                    Some("png"),
                    None,
                    None,
                    cancel,
                )
                .map_err(|e| format!("screenshot failed: {e}"))?;
            Ok((bytes, url))
        })
    }

    /// List tabs; with `adopt`, first pick up pages opened outside ChromeUse
    /// (the user's own tabs in an attached Chrome).
    pub fn list_tabs(&self, adopt: bool) -> Result<Vec<(ChromeTabId, String)>, String> {
        let mut guard = self.lock();
        let session = Self::session_mut(&mut guard)?;
        if adopt {
            match session.browser.pages(PAGES_TIMEOUT) {
                Ok(pages) => {
                    for page in pages {
                        let target_id = page.target_id();
                        if !session
                            .tabs
                            .values()
                            .any(|entry| entry.target_id == target_id)
                        {
                            let id = session.next_tab_id;
                            session.next_tab_id += 1;
                            session.tabs.insert(
                                id,
                                TabEntry {
                                    target_id,
                                    page,
                                    refs: HashSet::new(),
                                },
                            );
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "ChromeUse: tab adoption failed"),
            }
        }
        let mut tabs: Vec<(ChromeTabId, String)> = session
            .tabs
            .iter()
            .map(|(id, entry)| (*id, entry.page.url()))
            .collect();
        tabs.sort_by_key(|(id, _)| *id);
        Ok(tabs)
    }

    pub fn close_tab(&self, tab_id: ChromeTabId) -> Result<(), String> {
        let mut guard = self.lock();
        let session = Self::session_mut(&mut guard)?;
        let entry = session
            .tabs
            .remove(&tab_id)
            .ok_or_else(|| format!("unknown tab_id {tab_id}; list open tabs with ChromeUseTabs"))?;
        entry
            .page
            .close(Some(ACTION_TIMEOUT_MS), false)
            .map_err(|e| format!("closing tab {tab_id} failed: {e}"))
    }

    /// Close the whole session. An owned (launched) Chrome process is
    /// terminated; an attached browser is detached and left running.
    pub fn close_session(&self) -> Result<(), String> {
        let mut guard = self.lock();
        let Some(session) = guard.take() else {
            return Err("no Chrome session is open".into());
        };
        session
            .browser
            .close()
            .map_err(|e| format!("closing the Chrome session failed: {e}"))
    }

    fn close_session_quiet(&self) {
        let mut guard = self.lock();
        if let Some(session) = guard.take()
            && let Err(e) = session.browser.close()
        {
            tracing::warn!(error = %e, "ChromeUse shutdown: browser close failed");
        }
    }

    /// Block until `target` is satisfied or its budget expires.
    pub fn wait_for(
        &self,
        tab_id: ChromeTabId,
        target: WaitTarget,
        cancel: Option<&CancelToken>,
    ) -> Result<String, String> {
        match target {
            WaitTarget::Sleep(duration) => {
                let start = Instant::now();
                while start.elapsed() < duration {
                    if cancel.is_some_and(CancelToken::is_cancelled) {
                        return Err("operation cancelled".into());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(format!("Waited {:.1}s.", duration.as_secs_f64()))
            }
            WaitTarget::Appears(text) => self.poll_text(tab_id, &text, true, cancel),
            WaitTarget::Disappears(text) => self.poll_text(tab_id, &text, false, cancel),
        }
    }

    fn poll_text(
        &self,
        tab_id: ChromeTabId,
        text: &str,
        want_present: bool,
        cancel: Option<&CancelToken>,
    ) -> Result<String, String> {
        const BODY_TEXT_JS: &str = "(document.body && document.body.innerText) || \"\"";
        let start = Instant::now();
        self.with_tab(tab_id, |entry| {
            loop {
                if cancel.is_some_and(CancelToken::is_cancelled) {
                    return Err("operation cancelled".into());
                }
                let wire = entry
                    .page
                    .evaluate_with_cancel(BODY_TEXT_JS, None, Some(ACTION_TIMEOUT_MS), cancel)
                    .map_err(|e| format!("text probe failed: {e}"))?;
                let json = rustwright_core::decode_wire_value(&wire)
                    .map_err(|e| format!("text probe decode failed: {e}"))?;
                let body: String = serde_json::from_str(&json).unwrap_or_default();
                if body.contains(text) == want_present {
                    let state = if want_present {
                        "appeared"
                    } else {
                        "disappeared"
                    };
                    return Ok(format!("\"{text}\" {state}."));
                }
                if start.elapsed() >= WAIT_TIMEOUT {
                    let expectation = if want_present {
                        "to appear"
                    } else {
                        "to disappear"
                    };
                    return Err(format!(
                        "timed out after {}s waiting for \"{text}\" {expectation}",
                        WAIT_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(WAIT_POLL);
            }
        })
    }
}

/// What `ChromeUseWaitFor` blocks on.
pub enum WaitTarget {
    Appears(String),
    Disappears(String),
    Sleep(Duration),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_options_pin_headless_default_and_disable_telemetry() {
        let chrome = ChromeSettings::default();
        let value: serde_json::Value =
            serde_json::from_str(&launch_options(&chrome, None)).unwrap();
        // The engine's own default is headless; ChromeUse must override it.
        assert_eq!(value["headless"], serde_json::json!(false));
        assert_eq!(value["env"]["DISABLE_TELEMETRY"], serde_json::json!("1"));
        assert_eq!(value["chromium_sandbox"], serde_json::json!(false));
    }

    #[test]
    fn launch_options_honors_overrides_and_settings() {
        let chrome = ChromeSettings {
            executable: Some("/opt/chrome".into()),
            headless: true,
            user_data_dir: Some("/tmp/profile".into()),
            cdp_endpoint: None,
        };
        let value: serde_json::Value =
            serde_json::from_str(&launch_options(&chrome, None)).unwrap();
        assert_eq!(value["headless"], serde_json::json!(true));
        assert_eq!(value["executable_path"], serde_json::json!("/opt/chrome"));
        assert_eq!(value["user_data_dir"], serde_json::json!("/tmp/profile"));
        // A call-site override wins over the settings value.
        let overridden: serde_json::Value =
            serde_json::from_str(&launch_options(&chrome, Some(false))).unwrap();
        assert_eq!(overridden["headless"], serde_json::json!(false));
    }
    #[test]
    fn default_profile_dir_lives_under_the_manox_config_root() {
        let dir = default_profile_dir();
        assert!(dir.ends_with("chrome-profile"), "{dir}");
    }

    /// Extract the `[eN]` ref from the snapshot line containing `needle`.
    fn ref_of(snapshot: &str, needle: &str) -> String {
        let line = snapshot
            .lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing `{needle}` in snapshot:\n{snapshot}"));
        let start = line.rfind('[').expect("ref open") + 1;
        let end = line.rfind(']').expect("ref close");
        line[start..end].to_string()
    }

    const FIXTURE_HTML: &str = r#"<!doctype html>
<html>
  <head><title>ChromeUse Fixture</title></head>
  <body>
    <h1>ChromeUse Fixture</h1>
    <p id="status">idle</p>
    <button id="go" onclick="document.getElementById('status').textContent = 'clicked'">Press me</button>
    <input id="name" type="text" placeholder="Your name">
    <select id="color">
      <option value="red">Red</option>
      <option value="green">Green</option>
    </select>
  </body>
</html>
"#;

    /// End-to-end session against a real Chrome: headless launch → file://
    /// fixture → snapshot refs → click / type / select → screenshot → close.
    /// Network-independent (file:// URL). Run with:
    /// `MANOX_RUN_LIVE=1 cargo test -p agent chrome_use -- --ignored --nocapture`
    #[test]
    #[ignore = "requires MANOX_RUN_LIVE=1 and an installed Chrome/Chromium"]
    fn live_chrome_session_round_trip() {
        if std::env::var("MANOX_RUN_LIVE").is_err() {
            eprintln!("skipping: MANOX_RUN_LIVE not set");
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("manox-chrome-use-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fixture = dir.join("fixture.html");
        std::fs::write(&fixture, FIXTURE_HTML).unwrap();
        let url = format!("file://{}", fixture.display());

        let rt = runtime();
        // Close any session a prior test left behind, then launch headless
        // (the override applies because this starts the session).
        let _ = rt.close_session();
        rt.ensure_session(None, Some(true), None).expect("launch");

        let tab = rt.open_tab(Some(&url), None).expect("open tab");
        let snapshot = rt.snapshot(tab, None).expect("snapshot");
        assert!(snapshot.contains("ChromeUse Fixture"), "{snapshot}");
        let button_ref = ref_of(&snapshot, "Press me");
        let input_ref = ref_of(&snapshot, "Your name");
        let select_ref = ref_of(&snapshot, "combobox");

        // Click the button through its snapshot ref; the handler flips the
        // status paragraph, which the fresh snapshot must show.
        let selector = rt.ref_selector(tab, &button_ref).expect("button ref");
        let after_click = rt
            .with_tab(tab, |entry| {
                entry
                    .page
                    .click_with_cancel(&selector, Some(ACTION_TIMEOUT_MS), None)
                    .expect("click");
                entry.refs.clear();
                let (text, refs) = super::super::snapshot::take(&entry.page, None).expect("take");
                entry.refs = refs;
                Ok(text)
            })
            .expect("click round");
        assert!(after_click.contains("clicked"), "{after_click}");

        // A stale ref (issued before the click's snapshot) must fail fast.
        let stale = rt.ref_selector(tab, "e9999");
        assert!(stale.is_err(), "stale ref must not resolve");

        // Type into the text input and read the value back via snapshot.
        let selector = rt.ref_selector(tab, &input_ref).expect("input ref");
        let after_type = rt
            .with_tab(tab, |entry| {
                entry
                    .page
                    .fill_with_cancel(&selector, "manox", Some(ACTION_TIMEOUT_MS), None)
                    .expect("fill");
                entry.refs.clear();
                let (text, refs) = super::super::snapshot::take(&entry.page, None).expect("take");
                entry.refs = refs;
                Ok(text)
            })
            .expect("type round");
        assert!(after_type.contains("value=\"manox\""), "{after_type}");

        // Select an option by label.
        let selector = rt.ref_selector(tab, &select_ref).expect("select ref");
        let selected = rt
            .with_tab(tab, |entry| {
                entry
                    .page
                    .select_options_with_cancel(
                        &selector,
                        &["green".to_string()],
                        Some(ACTION_TIMEOUT_MS),
                        None,
                    )
                    .map_err(|e| format!("select failed: {e}"))
            })
            .expect("select round");
        assert_eq!(selected, vec!["green".to_string()]);

        // Screenshot: PNG magic and non-trivial size.
        let (bytes, shot_url) = rt.screenshot(tab, false, None).expect("screenshot");
        assert!(bytes.starts_with(b"\x89PNG"), "not a PNG");
        assert!(bytes.len() > 1024, "{}", bytes.len());
        assert!(shot_url.starts_with("file://"), "{shot_url}");

        rt.close_session().expect("close session");
        assert!(
            rt.close_session().is_err(),
            "second close must report no session"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
