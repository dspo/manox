//! Process-global host identity. Each host (the native app, the VS Code
//! extension) pins its identity before `agent::init`; sessions carry the
//! creating host in their header metadata so every host's session list
//! stays disjoint.

/// The host process that owns a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    ManoxApp,
    Vscode,
}

impl Host {
    /// Stable filesystem/wire slug, written into the session header
    /// `metadata.host`.
    pub const fn slug(self) -> &'static str {
        match self {
            Host::ManoxApp => "manox",
            Host::Vscode => "vscode",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Host> {
        match slug {
            "manox" => Some(Host::ManoxApp),
            "vscode" => Some(Host::Vscode),
            _ => None,
        }
    }
}

/// The host of this process. Set once at startup before `agent::init`;
/// defaults to the native app.
static CURRENT: std::sync::Mutex<Host> = std::sync::Mutex::new(Host::ManoxApp);

pub fn set_host(host: Host) {
    *CURRENT.lock().expect("host mutex poisoned") = host;
}

pub fn current() -> Host {
    *CURRENT.lock().expect("host mutex poisoned")
}

/// The host a session belongs to, read from its header `metadata.host`.
/// Untagged files (created before the host tag existed) belong to the
/// native app; an unrecognized slug belongs to no host.
pub fn session_host(metadata: Option<&serde_json::Value>) -> Option<Host> {
    match metadata.and_then(|m| m.get("host")) {
        // A present but non-string (or unrecognized) value belongs to no
        // host; only an absent key marks a pre-tag legacy file.
        Some(value) => value.as_str().and_then(Host::from_slug),
        None => Some(Host::ManoxApp),
    }
}

/// Whether a session is visible to the current host.
pub fn belongs_to_current_host(metadata: Option<&serde_json::Value>) -> bool {
    session_host(metadata) == Some(current())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_and_from_slug_round_trip() {
        for host in [Host::ManoxApp, Host::Vscode] {
            assert_eq!(Host::from_slug(host.slug()), Some(host));
        }
        assert_eq!(Host::from_slug("bogus"), None);
    }

    #[test]
    fn defaults_to_the_native_app() {
        assert_eq!(current(), Host::ManoxApp);
    }

    #[test]
    fn untagged_metadata_belongs_to_the_native_app() {
        assert_eq!(session_host(None), Some(Host::ManoxApp));
        assert_eq!(
            session_host(Some(&serde_json::json!({}))),
            Some(Host::ManoxApp)
        );
    }

    #[test]
    fn tagged_metadata_maps_to_its_host() {
        assert_eq!(
            session_host(Some(&serde_json::json!({ "host": "vscode" }))),
            Some(Host::Vscode)
        );
        // An unrecognized slug belongs to no host.
        assert_eq!(
            session_host(Some(&serde_json::json!({ "host": "bogus" }))),
            None
        );
        // A present but non-string value belongs to no host (fail-closed).
        assert_eq!(session_host(Some(&serde_json::json!({ "host": 42 }))), None);
    }

    #[test]
    fn membership_compares_against_the_current_host() {
        assert!(belongs_to_current_host(None));
        assert!(belongs_to_current_host(Some(
            &serde_json::json!({ "host": "manox" })
        )));
        assert!(!belongs_to_current_host(Some(
            &serde_json::json!({ "host": "vscode" })
        )));
        assert!(!belongs_to_current_host(Some(
            &serde_json::json!({ "host": "bogus" })
        )));
        set_host(Host::Vscode);
        assert!(belongs_to_current_host(Some(
            &serde_json::json!({ "host": "vscode" })
        )));
        assert!(!belongs_to_current_host(None));
        set_host(Host::ManoxApp);
    }
}
