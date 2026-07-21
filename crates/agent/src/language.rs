//! Shared language enumeration for the two manox language axes.
//!
//! [`Language`] is the single representation behind both the UI locale
//! (Fluent bundle selection, see [`crate::i18n`]) and the per-thread agent
//! language (which harness prompt assets and tool descriptions the thread
//! renders in, see [`crate::system_prompt`] / [`crate::prompt`]). Keeping one
//! type for both axes means a Chinese-UI / English-agent configuration (or the
//! reverse) is just two `Language` values carried independently, and the two
//! never bleed into each other.
//!
//! Configuration tokens are fixed to `"en"` / `"zh-CN"`; [`Language::from_token`]
//! rejects anything else so a malformed `settings.toml` surfaces loudly (the
//! caller warns and falls back to English at load time) rather than silently
//! coercing an unrelated tag.

use unic_langid::LanguageIdentifier;

/// The two languages manox speaks on either axis. `Copy` so it threads through
/// request-build and render call sites by value with no lifetime plumbing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Language {
    #[default]
    En,
    ZhCn,
}

impl Language {
    /// The canonical token written to `settings.toml` (`"en"` / `"zh-CN"`).
    /// Stable across builds; changing a token is a breaking config change.
    pub const fn token(self) -> &'static str {
        match self {
            Language::En => "en",
            Language::ZhCn => "zh-CN",
        }
    }

    /// Parse a config token. Accepts only the canonical [`Self::token`] forms;
    /// any other string is `None` so the caller can warn and fall back, rather
    /// than silently re-mapping an unrelated tag.
    pub fn from_token(s: &str) -> Option<Self> {
        match s.trim() {
            "en" => Some(Language::En),
            "zh-CN" => Some(Language::ZhCn),
            _ => None,
        }
    }

    /// English endonym injected into the system prompt so the model knows which
    /// language to address the user in. Always English regardless of locale —
    /// the model parses the directive, the user doesn't see this string.
    pub fn english_name(self) -> &'static str {
        match self {
            Language::En => "English",
            Language::ZhCn => "Simplified Chinese",
        }
    }

    /// Endonym shown verbatim in the settings dropdown. Fixed per language and
    /// never re-localized, so the picker reads `English` / `简体中文` whether the
    /// UI itself is currently English or Chinese.
    pub fn endonym(self) -> &'static str {
        match self {
            Language::En => "English",
            Language::ZhCn => "简体中文",
        }
    }

    /// BCP47 langid for Fluent bundle construction.
    pub fn langid(self) -> LanguageIdentifier {
        match self {
            Language::En => "en".parse().expect("en is a valid BCP47 langid"),
            Language::ZhCn => "zh-CN".parse().expect("zh-CN is a valid BCP47 langid"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trip() {
        for lang in [Language::En, Language::ZhCn] {
            assert_eq!(Language::from_token(lang.token()), Some(lang));
        }
    }

    #[test]
    fn from_token_rejects_non_canonical() {
        // Lenient variants (`zh`, `zh-Hans`, `en-US`) are intentionally not
        // accepted — the config token is fixed to the canonical form so a typo
        // surfaces at load rather than coercing silently.
        assert_eq!(Language::from_token("zh"), None);
        assert_eq!(Language::from_token("zh-Hans"), None);
        assert_eq!(Language::from_token("en-US"), None);
        assert_eq!(Language::from_token("fr"), None);
        assert_eq!(Language::from_token(""), None);
    }

    #[test]
    fn endonym_is_fixed_per_language() {
        assert_eq!(Language::En.endonym(), "English");
        assert_eq!(Language::ZhCn.endonym(), "简体中文");
    }

    #[test]
    fn english_name_for_prompt_directive() {
        assert_eq!(Language::En.english_name(), "English");
        assert_eq!(Language::ZhCn.english_name(), "Simplified Chinese");
    }
}
