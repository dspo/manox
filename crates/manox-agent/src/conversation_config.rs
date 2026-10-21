//! The object store as the conversation's configuration authority.
//!
//! [`ConversationConfigSource`] is the harness's single read surface for a
//! conversation's picks; this is the implementation the desktop app's engine
//! installs — one per conversation, keyed by session id, reading the
//! `conversations` row. The journal carries no configuration of its own, so
//! there is no second answer to disagree with this one.

use std::sync::Arc;

use manox_harness::ConversationConfigSource;
use manox_harness::sandbox::PermissionMode;

use crate::object_store::ObjectStore;

/// One conversation's row, as the engine's configuration source.
#[derive(Debug, Clone)]
pub struct ObjectConfigSource {
    session_id: String,
}

impl ObjectConfigSource {
    /// The source for one conversation. Installing this on the engine's
    /// permission gate makes the row the live authority every tool resolver
    /// reads per call.
    pub fn for_session(session_id: impl Into<String>) -> Arc<dyn ConversationConfigSource> {
        Arc::new(Self {
            session_id: session_id.into(),
        })
    }
}

impl ConversationConfigSource for ObjectConfigSource {
    fn model(&self) -> Option<manox_harness::types::Model> {
        let model_ref = self.model_ref()?;
        let registry = crate::provider_glue::global();
        manox_harness::model_ref::resolve_model_ref(&registry, &model_ref)
    }

    fn model_ref(&self) -> Option<String> {
        let row = ObjectStore::try_global()?.conversation(&self.session_id)?;
        let model_id = row.model_id?;
        // The exact registration the pick named wins: wire variants of one
        // model share a bare id, so a bare-id reference would resolve the
        // first registration instead of the picked one.
        Some(match row.model_provider.as_deref() {
            Some(provider) => format!("{provider}/{model_id}"),
            None => model_id,
        })
    }

    fn permission_mode(&self) -> Option<PermissionMode> {
        let row = ObjectStore::try_global()?.conversation(&self.session_id)?;
        row.permission_mode
            .as_deref()
            .and_then(PermissionMode::from_wire)
    }

    fn reasoning_effort(&self) -> Option<String> {
        let row = ObjectStore::try_global()?.conversation(&self.session_id)?;
        row.reasoning_effort
    }

    fn config_version(&self) -> i64 {
        ObjectStore::try_global()
            .and_then(|store| store.conversation(&self.session_id))
            .map(|row| row.config_version)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::TEST_LOCK;

    fn scratch() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        crate::object_store::set_path_for_test(Some(dir.path().join("objects.db")));
        crate::object_store::init();
        (dir, guard)
    }

    /// Every getter answers from the row, and an absent row answers `None`
    /// rather than an error: "this conversation has no pick" and "the read
    /// failed" must not be the same answer, or a reader would apply a default
    /// over a pick it merely could not see.
    #[test]
    fn every_getter_answers_from_the_row() {
        let (_dir, _lock) = scratch();
        let store = ObjectStore;
        let sid = "c-1";
        let source = ObjectConfigSource::for_session(sid);

        assert_eq!(source.permission_mode(), None);
        assert_eq!(source.reasoning_effort(), None);
        assert_eq!(source.model_ref(), None);
        assert_eq!(source.config_version(), 0);

        store.create_conversation(sid, None);
        store.set_permission_mode(sid, Some(PermissionMode::ReadOnly.wire()));
        store.set_reasoning_effort(sid, Some("max"));
        store.set_model(sid, Some("test-provider"), Some("test-model"));

        assert_eq!(source.permission_mode(), Some(PermissionMode::ReadOnly));
        assert_eq!(source.reasoning_effort().as_deref(), Some("max"));
        assert_eq!(
            source.model_ref().as_deref(),
            Some("test-provider/test-model")
        );
        assert!(
            source.config_version() >= 3,
            "each config write moves the version"
        );
    }

    /// An unknown permission string reads as "no pick" instead of failing:
    /// a hand-edited row must not take the gate down with it.
    #[test]
    fn an_unparseable_mode_reads_as_no_pick() {
        let (_dir, _lock) = scratch();
        let store = ObjectStore;
        store.create_conversation("c-1", None);
        store.set_permission_mode("c-1", Some("not-a-mode"));
        let source = ObjectConfigSource::for_session("c-1");
        assert_eq!(source.permission_mode(), None);
    }

    /// The model reference keeps the provider when the row carries one, so a
    /// wire variant of a shared model id resolves to the registration the
    /// user picked rather than the first one registered.
    #[test]
    fn the_model_ref_pins_the_picked_provider() {
        let (_dir, _lock) = scratch();
        let store = ObjectStore;
        store.create_conversation("c-1", None);
        store.set_model("c-1", Some("picked"), Some("shared-id"));
        let source = ObjectConfigSource::for_session("c-1");
        assert_eq!(source.model_ref().as_deref(), Some("picked/shared-id"));

        store.set_model("c-1", None, Some("bare-id"));
        assert_eq!(source.model_ref().as_deref(), Some("bare-id"));
    }
}
