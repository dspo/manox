// JSONL-based session storage.
//
// Each line in the file is a complete JSON object representing a single
// SessionTreeEntry. The file is append-only; new entries are written at
// the end. The leaf entry tracks the cursor position.

use crate::session::{SessionStorage, SessionTreeEntry};
use serde::{Deserialize, Serialize};

/// JSONL session storage backed by a file.
pub struct JsonlSessionStorage {
    // Placeholder — full implementation in Phase 3.
    _private: (),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonlSessionMetadata {
    pub id: String,
    pub cwd: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[async_trait::async_trait]
impl SessionStorage for JsonlSessionStorage {
    async fn create_entry_id(&self) -> Result<String, anyhow::Error> {
        Ok(uuid::Uuid::new_v4().to_string())
    }

    async fn append_entry(&self, _entry: &SessionTreeEntry) -> Result<(), anyhow::Error> {
        Ok(())
    }

    async fn get_entry(&self, _id: &str) -> Result<Option<SessionTreeEntry>, anyhow::Error> {
        Ok(None)
    }

    async fn get_leaf_id(&self) -> Result<Option<String>, anyhow::Error> {
        Ok(None)
    }

    async fn set_leaf_id(&self, _leaf_id: Option<&str>) -> Result<(), anyhow::Error> {
        Ok(())
    }

    async fn get_entries(&self) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        Ok(Vec::new())
    }

    async fn get_path_to_root_or_compaction(
        &self,
        _leaf_id: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        Ok(Vec::new())
    }
}