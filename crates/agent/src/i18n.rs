//! Process-global i18n via fluent, runtime-switchable on the UI axis.
//!
//! The [`Language`] type lives in [`crate::language`]; this module owns the
//! Fluent bundle machinery and the UI-locale global. The UI locale is no longer
//! frozen at `init` — [`set_ui_language`] swaps it live (rebuild bundles, refresh
//! every window, rebuild native menus) so a user picking a language in settings
//! sees it immediately. History notifications and user content are never
//! retroactively rewritten; only chrome re-localizes on the next render.
//!
//! The agent axis (which harness prompt / tool-description language a thread
//! uses) is a separate, per-thread immutable value on [`crate::thread::Thread`]
//! and never flows through here — model-facing prose is selected by that
//! thread-local value in [`crate::system_prompt`] / [`crate::prompt`], not by
//! this process-global UI locale, so a Chinese-UI / English-agent thread (or the
//! reverse) renders consistently and two threads in different agent languages
//! can run concurrently.

use std::cell::RefCell;
use std::sync::{OnceLock, RwLock};

use anyhow::Result;
use fluent::{FluentArgs, FluentBundle, FluentResource, FluentValue};
use gpui::{App, SharedString};

pub use crate::language::Language;

const EN_FTL: &str = include_str!("../locales/en.ftl");
const ZH_CN_FTL: &str = include_str!("../locales/zh-CN.ftl");

struct L10n {
    bundle: FluentBundle<FluentResource>,
    /// English fallback consulted only when `bundle` lacks the key. Fluent
    /// rejects duplicate message ids within a single bundle, so fallback is a
    /// separate bundle rather than a second resource in the primary one.
    fallback: Option<FluentBundle<FluentResource>>,
}

/// Current UI locale. `RwLock` (not `OnceLock`) so [`set_ui_language`] can swap
/// it at runtime; `FluentBundle` itself is `!Send` (its intl-memoizer holds a
/// `RefCell`), so each thread's bundle lives in a thread-local and is rebuilt
/// lazily against whatever locale this global reports when the thread next calls
/// [`t`].
static LANG: RwLock<Language> = RwLock::new(Language::En);

thread_local! {
    static L10N: RefCell<Option<L10n>> = const { RefCell::new(None) };
}

/// A menu rebuilder registered by the bin (which owns the native-menu
/// construction — `Quit` action and `Menu`/`MenuItem` live there). When the UI
/// locale changes the native menus must be re-`set_menus`'d with fresh
/// `t()`-resolved labels; this indirection keeps the rebuilder out of the
/// `agent` crate (it can't depend on the bin) without scattering menu-rebuild
/// calls across the UI layer.
type MenuRebuild = Box<dyn Fn(&mut App) + Send + Sync>;
static MENU_REBUILDER: OnceLock<MenuRebuild> = OnceLock::new();

/// Register the native-menu rebuilder. Called once from the bin at startup,
/// after [`init`]. Subsequent [`set_ui_language`] calls invoke it so menu labels
/// re-localize live.
pub fn set_menu_rebuilder(rebuild: impl Fn(&mut App) + Send + Sync + 'static) {
    let _ = MENU_REBUILDER.set(Box::new(rebuild));
}

/// Read `settings.toml`, resolve the UI locale, and build the bundle on the
/// calling (startup) thread. Called once from `agent::init` before any UI
/// render or system-prompt build. Any failure is non-fatal: warn and fall back
/// to English so a malformed config never blocks startup.
pub fn init() {
    let lang = crate::settings::load().resolve().ui;
    *LANG.write().expect("i18n LANG lock poisoned") = lang;
    if init_with(lang).is_err() {
        tracing::warn!("i18n bundle build failed for {lang:?}; falling back to English");
        *LANG.write().expect("i18n LANG lock poisoned") = Language::En;
        let _ = init_with(Language::En);
    }
}

/// Swap the UI locale live: update the global, drop every thread's cached
/// bundle so the next [`t`] rebuilds against the new locale, refresh all
/// windows so chrome re-renders, and rebuild native menus via the registered
/// rebuilder. Existing notifications and user content are not rewritten — only
/// chrome re-localizes from this point forward.
///
/// Callers must have already persisted the new locale to `settings.toml` (or be
/// the persist path itself); this function touches only in-memory state and UI.
pub fn set_ui_language(lang: Language, cx: &mut App) {
    *LANG.write().expect("i18n LANG lock poisoned") = lang;
    // Drop this thread's bundle; other threads' bundles rebuild lazily on
    // their next `t()` call, reading the new locale from `LANG`.
    L10N.with(|cell| *cell.borrow_mut() = None);
    cx.refresh_windows();
    if let Some(rebuild) = MENU_REBUILDER.get() {
        rebuild(cx);
    }
}

fn primary_resource(lang: Language) -> &'static str {
    match lang {
        Language::En => EN_FTL,
        Language::ZhCn => ZH_CN_FTL,
    }
}

fn init_with(lang: Language) -> Result<()> {
    let primary = FluentResource::try_new(primary_resource(lang).to_string())
        .map_err(|(_, errs)| anyhow::anyhow!("primary locale resource parse failed: {errs:?}"))?;
    let mut bundle = FluentBundle::new(vec![lang.langid()]);
    // manox is a code editor (LTR); the bidi isolate marks fluent wraps
    // around variables would leak U+2068/2069 into rendered strings and break
    // substring matching in tests / copy-paste.
    bundle.set_use_isolating(false);
    bundle
        .add_resource(primary)
        .map_err(|errs| anyhow::anyhow!("primary resource add errors: {errs:?}"))?;
    // English fallback as a separate bundle — fluent rejects duplicate
    // message ids within one bundle, so a partial translation defers to en
    // via a lookup chain rather than a second resource in the primary bundle.
    let fallback = if lang != Language::En {
        let res = FluentResource::try_new(EN_FTL.to_string())
            .map_err(|(_, errs)| anyhow::anyhow!("en fallback resource parse failed: {errs:?}"))?;
        let mut fb = FluentBundle::new(vec![Language::En.langid()]);
        fb.set_use_isolating(false);
        fb.add_resource(res)
            .map_err(|errs| anyhow::anyhow!("fallback resource add errors: {errs:?}"))?;
        Some(fb)
    } else {
        None
    };
    let l10n = L10n { bundle, fallback };
    L10N.with(|cell| *cell.borrow_mut() = Some(l10n));
    Ok(())
}

/// Build the thread-local bundle for the current thread if not already built,
/// or rebuild it if the cached locale drifts from the live global (e.g. after
/// [`set_ui_language`] on another thread). Returns the resolved locale.
fn ensure_init() -> Language {
    let lang = read_lang();
    L10N.with(|cell| {
        let needs_rebuild = cell
            .borrow()
            .as_ref()
            .is_none_or(|l| l.bundle.locales.first() != Some(&lang.langid()));
        if needs_rebuild && let Err(e) = init_with(lang) {
            // A corrupt .ftl would otherwise fall through to key-verbatim
            // rendering in `format` with no trace; surface it here so the
            // failure is diagnosable in the field.
            tracing::warn!(error = %e, ?lang, "i18n bundle rebuild failed");
        }
    });
    lang
}

/// Current UI language. Returns [`Language::default`] (English) before `init`
/// or on lock poisoning, so callers during early startup still get a sane answer.
pub fn current() -> Language {
    read_lang()
}

/// Read the live UI locale, falling back to English on a poisoned lock (a
/// panicking lock holder is unrecoverable, so defaulting keeps startup robust
/// rather than propagating the poisoning).
fn read_lang() -> Language {
    LANG.read().map(|g| *g).unwrap_or(Language::En)
}

/// Resolve `key` with no arguments. Missing keys render as the key itself so
/// leaks surface during development rather than silently empty strings.
pub fn t(key: &str) -> SharedString {
    format(key, None)
}

/// Resolve `key` with string arguments (e.g. `workspace-unknown-command`'s
/// `$name`). Arguments borrow the caller's slices for the duration of the call.
pub fn t_str(key: &str, args: &[(&str, &str)]) -> SharedString {
    let mut fa = FluentArgs::new();
    for (k, v) in args {
        fa.set(*k, FluentValue::String(std::borrow::Cow::Borrowed(*v)));
    }
    format(key, Some(&fa))
}

/// Resolve `key` with a numeric `$count` argument, used for plural-aware
/// strings like relative time formatting.
pub fn t_count(key: &str, count: i64) -> SharedString {
    let mut fa = FluentArgs::new();
    fa.set("count", FluentValue::from(count));
    format(key, Some(&fa))
}

/// Resolve `key` with string arguments plus a numeric `$count`, for
/// plural-aware messages that also carry named string args.
pub fn t_str_count(key: &str, args: &[(&str, &str)], count: i64) -> SharedString {
    let mut fa = FluentArgs::new();
    for (k, v) in args {
        fa.set(*k, FluentValue::String(std::borrow::Cow::Borrowed(*v)));
    }
    fa.set("count", FluentValue::from(count));
    format(key, Some(&fa))
}

fn format(key: &str, args: Option<&FluentArgs>) -> SharedString {
    ensure_init();
    L10N.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(l10n) = guard.as_mut() else {
            return SharedString::from(key);
        };
        if let Some(s) = format_in(&mut l10n.bundle, key, args) {
            return s;
        }
        if let Some(fb) = l10n.fallback.as_mut()
            && let Some(s) = format_in(fb, key, args)
        {
            return s;
        }
        SharedString::from(key)
    })
}

/// Format `key` against a single bundle. Returns `None` if the key is absent
/// or the message has no value (so the caller can try the fallback bundle).
fn format_in(
    bundle: &mut FluentBundle<FluentResource>,
    key: &str,
    args: Option<&FluentArgs>,
) -> Option<SharedString> {
    let msg = bundle.get_message(key)?;
    let value = msg.value()?;
    let mut errors = vec![];
    let formatted = bundle.format_pattern(value, args, &mut errors);
    if !errors.is_empty() {
        tracing::warn!(key, ?errors, "fluent format errors");
    }
    Some(SharedString::from(formatted.into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `t` / `t_count` read the process-global `LANG`, so any test that flips
    /// it must hold this lock for its whole body — otherwise a parallel sibling
    /// reassigning `LANG` mid-call makes `ensure_init` rebuild against the
    /// wrong locale and the assertion sees the other language's string.
    static TEST_LANG_LOCK: Mutex<()> = Mutex::new(());

    fn set_lang(lang: Language) {
        *LANG.write().expect("i18n LANG lock poisoned") = lang;
        init_with(lang).expect("init_with should succeed for tests");
    }

    #[test]
    fn missing_key_returns_key() {
        let _g = TEST_LANG_LOCK.lock().unwrap();
        set_lang(Language::En);
        let v = t("does-not-exist-xyz");
        assert_eq!(v.as_ref(), "does-not-exist-xyz");
    }

    #[test]
    fn en_plural_minutes() {
        let _g = TEST_LANG_LOCK.lock().unwrap();
        set_lang(Language::En);
        assert_eq!(t_count("sidebar-time-minutes", 1).as_ref(), "1 minute ago");
        assert_eq!(t_count("sidebar-time-minutes", 5).as_ref(), "5 minutes ago");
    }

    #[test]
    fn zh_cn_no_plural_minutes() {
        let _g = TEST_LANG_LOCK.lock().unwrap();
        set_lang(Language::ZhCn);
        assert_eq!(t_count("sidebar-time-minutes", 1).as_ref(), "1 分钟前");
        assert_eq!(t_count("sidebar-time-minutes", 5).as_ref(), "5 分钟前");
    }

    #[test]
    fn string_arg_interpolation() {
        let _g = TEST_LANG_LOCK.lock().unwrap();
        set_lang(Language::En);
        let v = t_str("workspace-unknown-command", &[("name", "foo")]);
        assert!(v.contains("/foo"), "got: {v}");
        assert!(v.contains("Unknown command"), "got: {v}");
    }

    #[test]
    fn current_reflects_set_language() {
        let _g = TEST_LANG_LOCK.lock().unwrap();
        set_lang(Language::En);
        assert_eq!(current(), Language::En);
        set_lang(Language::ZhCn);
        assert_eq!(current(), Language::ZhCn);
    }
}
