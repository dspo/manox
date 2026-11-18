//! The manox state home (`MANOX_HOME`, else `$HOME/.manox`).
//!
//! The harness sits *below* the host crate — `manox-agent` depends on
//! `manox-harness`, so neither the kernel nor the extension layer may call
//! `manox_agent::paths::manox_home()`. This module is the harness-side
//! authority for the same contract: `MANOX_HOME` overrides the root
//! wholesale, an empty value is ignored, and `$HOME/.manox` is the default.
//! Every harness path that lands in the state root resolves through here, so
//! an embedder that relocates the root moves all of them together.
//!
//! Two entry points exist because callers have historically differed when no
//! home can be resolved at all: [`manox_home`] falls back to the platform
//! passwd entry (`dirs::home_dir`), while [`manox_home_from_env`] reports
//! `None` so a session path can degrade to the process cwd instead.

use std::ffi::OsString;
use std::path::PathBuf;

/// The state root from the environment alone: `MANOX_HOME` when set to a
/// non-empty value, else `$HOME/.manox`. `None` when neither is usable.
pub(crate) fn manox_home_from_env() -> Option<PathBuf> {
    resolve(std::env::var_os("MANOX_HOME"), std::env::var_os("HOME"))
}

/// The state root with the platform home as the last resort — the shape path
/// consumers use where a passwd fallback is acceptable.
pub(crate) fn manox_home() -> Option<PathBuf> {
    manox_home_from_env().or_else(|| dirs::home_dir().map(|home| home.join(".manox")))
}

/// The precedence rule, pure so its tests need no process-wide env mutation.
fn resolve(manox_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    fn non_empty(value: OsString) -> Option<PathBuf> {
        if value.is_empty() {
            None
        } else {
            Some(PathBuf::from(value))
        }
    }
    manox_home
        .and_then(non_empty)
        .or_else(|| home.and_then(non_empty).map(|home| home.join(".manox")))
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::Mutex;

    /// Serializes tests that mutate `MANOX_HOME`: env vars are process-wide,
    /// so every test that points the state root at a scratch path holds this
    /// (and every test that resolves the root without mutating it holds it
    /// too, so it never observes another test's override).
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Sets `MANOX_HOME` for the guard's lifetime and restores the previous
    /// value on drop. Callers hold [`ENV_LOCK`] for the whole test.
    pub(crate) struct ManoxHomeGuard {
        previous: Option<OsString>,
    }

    impl ManoxHomeGuard {
        pub(crate) fn set(root: &Path) -> Self {
            let previous = std::env::var_os("MANOX_HOME");
            // SAFETY: the caller holds ENV_LOCK, so no other test mutates or
            // relies on a stable MANOX_HOME while this guard is alive.
            unsafe { std::env::set_var("MANOX_HOME", root) };
            Self { previous }
        }
    }

    impl Drop for ManoxHomeGuard {
        fn drop(&mut self) {
            match &self.previous {
                // SAFETY: restores the pre-test value under the same ENV_LOCK.
                Some(value) => unsafe { std::env::set_var("MANOX_HOME", value) },
                None => unsafe { std::env::remove_var("MANOX_HOME") },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manox_home_wins_over_home() {
        assert_eq!(
            resolve(Some("/state".into()), Some("/home/me".into())),
            Some(PathBuf::from("/state"))
        );
    }

    #[test]
    fn empty_manox_home_falls_back_to_home_dot_manox() {
        assert_eq!(
            resolve(Some(OsString::new()), Some("/home/me".into())),
            Some(PathBuf::from("/home/me/.manox"))
        );
    }

    #[test]
    fn no_usable_home_resolves_to_none() {
        assert_eq!(resolve(None, None), None);
        assert_eq!(resolve(Some(OsString::new()), Some(OsString::new())), None);
    }
}
