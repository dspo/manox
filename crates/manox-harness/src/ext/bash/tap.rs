//! Process-level observer for raw bash output chunks (#1030).
//!
//! The embedder installs one [`BashOutputTap`] before the first session
//! starts; every bash execution path then hands its raw bytes to it as they
//! leave the execution machinery. The tap is a **pure observer**: it must not
//! alter results, exit codes, or tool-visible output, and its semantics (e.g.
//! feeding a headless terminal-emulation core) live entirely on the embedder
//! side — the kernel stays protocol-agnostic.
//!
//! Coverage is guaranteed by construction at two choke points: foreground
//! variants compose the tap into the tool's `on_data` slot (tracked /
//! plain / persistent / subagent), and background tasks fire it where the
//! drain loop appends raw chunks (the spawn branches and their adopted
//! derivatives all funnel through the same drain). A foreground call that
//! owns a tracked task is the one tap for its window — the drain skips
//! chunks still owned by a foreground receiver, so every chunk leaves the
//! execution machinery exactly once.

use std::sync::{Arc, OnceLock};

/// The routing identity of one byte chunk.
///
/// `session_id` routes the bytes to the owning session's consumer (absent
/// only for harness-standalone assemblies that have no session identity);
/// `exec_id` distinguishes concurrent executions — a tool-call id in the
/// foreground, a `bg_<ordinal>` task id in the background. `exec_id` is
/// advisory only: parent and child sessions mint tool-call ids of the same
/// shape and they can collide across sessions, so consumers must not treat
/// it as a unique key — routing keys on `session_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashOutputTapId {
    pub session_id: Option<String>,
    pub exec_id: String,
}

/// A pure observer of raw bash output chunks. See the [module
/// documentation](self) for the contract.
pub type BashOutputTap = Arc<dyn Fn(BashOutputTapId, &[u8]) + Send + Sync>;

/// Why a tap installation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallError {
    /// A tap is already installed for this process. The tap is
    /// process-singleton on purpose: a second install would silently
    /// re-route bytes away from the first embedder.
    AlreadyInstalled,
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::AlreadyInstalled => {
                write!(f, "a bash output tap is already installed for this process")
            }
        }
    }
}

static TAP: OnceLock<BashOutputTap> = OnceLock::new();

/// Install the process-singleton output tap.
///
/// Same discipline as the AHP runtime builder: last-wins is deliberately
/// not offered, the install must precede the first session start, and a
/// second call is refused loudly.
pub fn install_bash_output_tap(tap: BashOutputTap) -> Result<(), InstallError> {
    TAP.set(tap).map_err(|_| InstallError::AlreadyInstalled)
}

/// The installed tap, if any. Snapshot per call site; firing it must never
/// observe a torn state (the `OnceLock` guarantees the closure outlives the
/// process).
pub(crate) fn current() -> Option<&'static BashOutputTap> {
    TAP.get()
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared recording tap for the acceptance tests. Installed once per
    //! test binary (`get_or_init`); every test asserts only the records
    //! carrying its own unique session id, so parallel tests never see each
    //! other's bytes. Non-tap tests that happen to run bash get their output
    //! recorded too — nobody reads those records.

    use super::{BashOutputTapId, install_bash_output_tap};
    use std::sync::{Arc, Mutex, OnceLock};

    #[derive(Debug, Clone)]
    pub(crate) struct Record {
        pub id: BashOutputTapId,
        pub bytes: Vec<u8>,
    }

    fn store() -> &'static Mutex<Vec<Record>> {
        static STORE: OnceLock<Mutex<Vec<Record>>> = OnceLock::new();
        STORE.get_or_init(|| Mutex::new(Vec::new()))
    }

    pub(crate) fn install() -> &'static Mutex<Vec<Record>> {
        static INSTALL: OnceLock<()> = OnceLock::new();
        INSTALL.get_or_init(|| {
            let _ = install_bash_output_tap(Arc::new(|id, bytes| {
                store()
                    .lock()
                    .expect("tap record store lock poisoned")
                    .push(Record {
                        id,
                        bytes: bytes.to_vec(),
                    });
            }));
        });
        store()
    }

    /// All records for one session, concatenated in arrival order.
    pub(crate) fn records_for(store: &Mutex<Vec<Record>>, session_id: &str) -> Vec<Record> {
        store
            .lock()
            .expect("tap record store lock poisoned")
            .iter()
            .filter(|r| r.id.session_id.as_deref() == Some(session_id))
            .cloned()
            .collect()
    }

    /// Drop every record for `session_id` (setup-time hygiene when a fixed
    /// session id could collide with an earlier test in the same binary).
    pub(crate) fn retain_except(store: &Mutex<Vec<Record>>, session_id: &str) {
        store
            .lock()
            .expect("tap record store lock poisoned")
            .retain(|r| r.id.session_id.as_deref() != Some(session_id));
    }

    /// #1030 (6): the process-singleton discipline — a second install is
    /// refused (the recording tap stays in place), and with no tap installed
    /// the composition is a no-op by construction (`current()` returning
    /// `None` leaves the caller's slot byte-identical; the untouched
    /// surrounding suite is the behavioral witness).
    #[test]
    fn second_install_is_refused() {
        install();
        assert_eq!(
            super::install_bash_output_tap(Arc::new(|_, _| ())),
            Err(super::InstallError::AlreadyInstalled)
        );
    }
}
