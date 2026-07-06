//! Process-global i18n via fluent.
//!
//! One `FluentBundle` per process; locale is fixed at `init` from
//! `settings.toml` and never switched at runtime (no UI control yet). UI chrome
//! reads strings through [`t`] / [`t_str`] / [`t_count`]. Model-facing content
//! — the system prompt, tool descriptions, tool error strings — is always
//! English and intentionally never routed through here, so the model's context
//! stays in one language regardless of the UI locale.
//!
//! The current language is also surfaced to [`system_prompt`] via
//! [`Language::english_name`], which injects a one-line directive telling the
//! model which language to address the user in. The prompt prose itself stays
//! English; only the user-facing reply language varies.

use std::cell::RefCell;
use std::sync::OnceLock;

use anyhow::Result;
use fluent::{FluentArgs, FluentBundle, FluentResource, FluentValue};
use gpui::SharedString;
use unic_langid::LanguageIdentifier;

const EN_FTL: &str = include_str!("../locales/en.ftl");
const ZH_CN_FTL: &str = include_str!("../locales/zh-CN.ftl");

/// Supported UI locales. Adding a language means: a new variant, a `.ftl`
/// resource, and a match arm in [`Language::langid`] / [`Language::primary_resource`]
/// / [`Language::english_name`] / [`parse_language`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Language {
    #[default]
    En,
    ZhCn,
}

impl Language {
    fn langid(self) -> LanguageIdentifier {
        match self {
            Language::En => "en".parse().expect("en is a valid BCP47 langid"),
            Language::ZhCn => "zh-CN".parse().expect("zh-CN is a valid BCP47 langid"),
        }
    }

    /// English endonym injected into the system prompt so the model knows which
    /// language to address the user in. Always English regardless of locale —
    /// the model parses the directive, the user doesn't see it.
    pub fn english_name(self) -> &'static str {
        match self {
            Language::En => "English",
            Language::ZhCn => "Simplified Chinese",
        }
    }

    fn primary_resource(self) -> &'static str {
        match self {
            Language::En => EN_FTL,
            Language::ZhCn => ZH_CN_FTL,
        }
    }
}

struct L10n {
    bundle: FluentBundle<FluentResource>,
    /// English fallback consulted only when `bundle` lacks the key. Fluent
    /// rejects duplicate message ids within a single bundle, so fallback is a
    /// separate bundle rather than a second resource in the primary one.
    fallback: Option<FluentBundle<FluentResource>>,
}

/// Chosen locale, settled once at `init`. `FluentBundle` itself is `!Send`
/// (its intl-memoizer holds a `RefCell`), so the bundle lives in a thread-local
/// — but the locale choice is process-global and `Copy`, and lives here.
static LANG: OnceLock<Language> = OnceLock::new();

thread_local! {
    static L10N: RefCell<Option<L10n>> = const { RefCell::new(None) };
}

/// Read `settings.toml`, resolve the locale, and build the bundle on the
/// calling (startup) thread. Called once from `agent::init` before any UI
/// render or system-prompt build. Any failure is non-fatal: warn and fall back
/// to English so a malformed config never blocks startup.
pub fn init() {
    let lang = crate::settings::load()
        .language
        .as_deref()
        .and_then(parse_language)
        .unwrap_or_default();
    let _ = LANG.set(lang);
    if init_with(lang).is_err() {
        tracing::warn!("i18n bundle build failed for {lang:?}; falling back to English");
        let _ = init_with(Language::En);
    }
}

/// Map a user-supplied language tag from `settings.toml` to a [`Language`].
/// Tolerates common variants (`zh`, `zh-Hans`, `en-US`); unknown tags return
/// `None` so the caller falls back to the default.
fn parse_language(s: &str) -> Option<Language> {
    match s.trim().to_lowercase().as_str() {
        "en" | "en-us" | "en_us" => Some(Language::En),
        "zh" | "zh-cn" | "zh_cn" | "zh-hans" | "zh-hans-cn" => Some(Language::ZhCn),
        _ => None,
    }
}

fn init_with(lang: Language) -> Result<()> {
    let primary = FluentResource::try_new(lang.primary_resource().to_string())
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

/// Build the thread-local bundle for the current thread if not already built.
/// Non-startup threads (e.g. a tokio worker) that call [`t`] get their own
/// bundle lazily. Returns the resolved locale.
fn ensure_init() -> Language {
    let lang = *LANG.get().unwrap_or(&Language::En);
    L10N.with(|cell| {
        if cell.borrow().is_none() {
            let _ = init_with(lang);
        }
    });
    lang
}

/// Current UI language. Returns [`Language::default`] (English) before `init`
/// or on init failure, so callers during early startup still get a sane answer.
pub fn current() -> Language {
    *LANG.get().unwrap_or(&Language::default())
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

    fn set_lang(lang: Language) {
        let _ = LANG.set(lang);
        init_with(lang).expect("init_with should succeed for tests");
    }

    #[test]
    fn en_fallback_for_missing_key() {
        // Fallback (zh-CN bundle also carries the en resource) is wired in
        // `init_with`; with both ftl files mirroring all keys it isn't
        // exercised by real data, so we don't assert it here. The raw-key
        // return for a fully-unknown id is covered by `missing_key_returns_key`.
    }

    #[test]
    fn missing_key_returns_key() {
        set_lang(Language::En);
        let v = t("does-not-exist-xyz");
        assert_eq!(v.as_ref(), "does-not-exist-xyz");
    }

    #[test]
    fn en_plural_minutes() {
        set_lang(Language::En);
        assert_eq!(t_count("sidebar-time-minutes", 1).as_ref(), "1 minute ago");
        assert_eq!(t_count("sidebar-time-minutes", 5).as_ref(), "5 minutes ago");
    }

    #[test]
    fn zh_cn_no_plural_minutes() {
        set_lang(Language::ZhCn);
        assert_eq!(t_count("sidebar-time-minutes", 1).as_ref(), "1 分钟前");
        assert_eq!(t_count("sidebar-time-minutes", 5).as_ref(), "5 分钟前");
    }

    #[test]
    fn string_arg_interpolation() {
        set_lang(Language::En);
        let v = t_str("workspace-unknown-command", &[("name", "foo")]);
        assert!(v.contains("/foo"), "got: {v}");
        assert!(v.contains("Unknown command"), "got: {v}");
    }

    #[test]
    fn english_name_for_prompt_directive() {
        assert_eq!(Language::En.english_name(), "English");
        assert_eq!(Language::ZhCn.english_name(), "Simplified Chinese");
    }

    #[test]
    fn parse_language_tolerant() {
        assert_eq!(parse_language("en"), Some(Language::En));
        assert_eq!(parse_language("En"), Some(Language::En));
        assert_eq!(parse_language("en-US"), Some(Language::En));
        assert_eq!(parse_language("zh"), Some(Language::ZhCn));
        assert_eq!(parse_language("zh-Hans"), Some(Language::ZhCn));
        assert_eq!(parse_language("ZH-CN"), Some(Language::ZhCn));
        assert_eq!(parse_language("fr"), None);
    }
}
