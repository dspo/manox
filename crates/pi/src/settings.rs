// Settings management with global/project scope merging.
//
// Settings are loaded from JSON files: global settings from ~/.pi/agent/settings.json
// and project-level overrides from .pi/settings.json in the project root.

use serde::{Deserialize, Serialize};

/// Project-scope compaction overrides.
///
/// Every field is optional, so a settings file only carries the values it
/// explicitly sets; the merge applies those field-wise over the base,
/// mirroring the recursive merge of TS `deepMergeSettings` instead of
/// replacing the whole section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompactionOverrides {
    /// Whether compaction is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Tokens to reserve for the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<usize>,
    /// Tokens to keep from the recent conversation tail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<usize>,
}

impl CompactionOverrides {
    /// Fold another override set into this one; explicitly set fields win.
    fn merge_from(&mut self, other: &CompactionOverrides) {
        if other.enabled.is_some() {
            self.enabled = other.enabled;
        }
        if other.reserve_tokens.is_some() {
            self.reserve_tokens = other.reserve_tokens;
        }
        if other.keep_recent_tokens.is_some() {
            self.keep_recent_tokens = other.keep_recent_tokens;
        }
    }

    /// Apply the explicitly set fields over `base`.
    fn apply_to(&self, base: &mut crate::compaction::CompactionSettings) {
        if let Some(enabled) = self.enabled {
            base.enabled = enabled;
        }
        if let Some(reserve_tokens) = self.reserve_tokens {
            base.reserve_tokens = reserve_tokens;
        }
        if let Some(keep_recent_tokens) = self.keep_recent_tokens {
            base.keep_recent_tokens = keep_recent_tokens;
        }
    }
}

/// Application settings with global and project-level overrides.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    /// Default provider to use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    /// Default model to use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Default thinking level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_thinking_level: Option<String>,
    /// Compaction overrides; absent leaves the other scope's values
    /// untouched on merge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionOverrides>,
    /// Whether to show cache miss notices; absent leaves the other scope's
    /// value untouched on merge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_cache_miss_notices: Option<bool>,
    /// Shell command prefix (e.g. "sudo").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_command_prefix: Option<String>,
    /// External editor path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_editor: Option<String>,
}

impl Settings {
    /// Merge project-level overrides into global settings.
    /// Project settings take precedence for explicitly set values only.
    pub fn merge(&mut self, project: &Settings) {
        if project.default_provider.is_some() {
            self.default_provider = project.default_provider.clone();
        }
        if project.default_model.is_some() {
            self.default_model = project.default_model.clone();
        }
        if project.default_thinking_level.is_some() {
            self.default_thinking_level = project.default_thinking_level.clone();
        }
        if project.shell_command_prefix.is_some() {
            self.shell_command_prefix = project.shell_command_prefix.clone();
        }
        if project.external_editor.is_some() {
            self.external_editor = project.external_editor.clone();
        }
        if let Some(overrides) = &project.compaction {
            self.compaction
                .get_or_insert_with(Default::default)
                .merge_from(overrides);
        }
        if project.show_cache_miss_notices.is_some() {
            self.show_cache_miss_notices = project.show_cache_miss_notices;
        }
    }

    /// The compaction settings with defaults under the explicit overrides.
    pub fn resolved_compaction(&self) -> crate::compaction::CompactionSettings {
        let mut resolved = crate::compaction::CompactionSettings::default();
        if let Some(overrides) = &self.compaction {
            overrides.apply_to(&mut resolved);
        }
        resolved
    }

    /// Load settings from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Serialize settings to a JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// The global settings file under `global_dir` (TS `~/.pi/agent/`).
    pub fn global_path(global_dir: &std::path::Path) -> std::path::PathBuf {
        global_dir.join("settings.json")
    }

    /// The project settings file under a project root (TS `.pi/settings.json`).
    pub fn project_path(project_root: &std::path::Path) -> std::path::PathBuf {
        project_root.join(".pi").join("settings.json")
    }

    /// Load the global settings file; a missing file reads as defaults.
    pub async fn load_global_at(global_dir: &std::path::Path) -> Result<Self, anyhow::Error> {
        load_file(&Self::global_path(global_dir)).await
    }

    /// Load the project settings file; a missing file reads as defaults.
    pub async fn load_project_at(project_root: &std::path::Path) -> Result<Self, anyhow::Error> {
        load_file(&Self::project_path(project_root)).await
    }

    /// Load global then project settings, merging project overrides on top.
    pub async fn load_at(
        project_root: &std::path::Path,
        global_dir: &std::path::Path,
    ) -> Result<Self, anyhow::Error> {
        let mut settings = Self::load_global_at(global_dir).await?;
        let project = Self::load_project_at(project_root).await?;
        settings.merge(&project);
        Ok(settings)
    }

    /// Reload from disk, re-merging the project overrides.
    pub async fn reload_at(
        &mut self,
        project_root: &std::path::Path,
        global_dir: &std::path::Path,
    ) -> Result<(), anyhow::Error> {
        *self = Self::load_at(project_root, global_dir).await?;
        Ok(())
    }

    /// Save to a JSON file, creating the parent directory.
    pub async fn save(&self, path: &std::path::Path) -> Result<(), anyhow::Error> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, self.to_json()?).await?;
        Ok(())
    }
}

/// Read and parse a settings file; `NotFound` reads as defaults, any other
/// failure surfaces (a corrupt settings file must not silently reset config).
async fn load_file(path: &std::path::Path) -> Result<Settings, anyhow::Error> {
    match tokio::fs::read_to_string(path).await {
        Ok(json) => Settings::from_json(&json)
            .map_err(|e| anyhow::anyhow!("invalid settings file {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_project_overrides() {
        let mut global = Settings {
            default_model: Some("claude-sonnet-4-6".into()),
            default_thinking_level: Some("medium".into()),
            ..Default::default()
        };

        let project = Settings {
            default_model: Some("claude-opus-4-8".into()),
            ..Default::default()
        };

        global.merge(&project);
        assert_eq!(global.default_model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(global.default_thinking_level.as_deref(), Some("medium"));
    }

    #[test]
    fn test_default_settings() {
        let settings = Settings::default();
        assert!(settings.default_model.is_none());
        assert!(settings.resolved_compaction().enabled);
    }

    #[test]
    fn test_merge_keeps_global_compaction_when_project_is_silent() {
        let mut global = Settings {
            compaction: Some(CompactionOverrides {
                enabled: Some(false),
                keep_recent_tokens: Some(5000),
                ..Default::default()
            }),
            show_cache_miss_notices: Some(true),
            ..Default::default()
        };
        // A project file that never mentions compaction or notices.
        let project = Settings::from_json(r#"{"default_model": "gpt-5"}"#).unwrap();

        global.merge(&project);
        let compaction = global.compaction.unwrap();
        assert_eq!(compaction.enabled, Some(false));
        assert_eq!(compaction.keep_recent_tokens, Some(5000));
        assert_eq!(global.show_cache_miss_notices, Some(true));
    }

    #[test]
    fn test_merge_applies_only_the_explicitly_set_compaction_fields() {
        let mut global = Settings {
            compaction: Some(CompactionOverrides {
                enabled: Some(false),
                reserve_tokens: Some(1000),
                keep_recent_tokens: Some(5000),
            }),
            ..Default::default()
        };
        let project =
            Settings::from_json(r#"{"compaction": {"keep_recent_tokens": 8000}}"#).unwrap();

        global.merge(&project);
        // The explicit field overrides; the untouched ones survive.
        let resolved = global.resolved_compaction();
        assert!(!resolved.enabled);
        assert_eq!(resolved.reserve_tokens, 1000);
        assert_eq!(resolved.keep_recent_tokens, 8000);
    }

    #[tokio::test]
    async fn test_save_and_reload_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let global_dir = dir.path().join("agent");
        let project = dir.path().join("proj");
        tokio::fs::create_dir_all(&project).await.unwrap();

        let settings = Settings {
            default_model: Some("claude-sonnet-4-6".into()),
            compaction: Some(CompactionOverrides {
                keep_recent_tokens: Some(5000),
                ..Default::default()
            }),
            ..Default::default()
        };
        settings
            .save(&Settings::global_path(&global_dir))
            .await
            .unwrap();

        let loaded = Settings::load_global_at(&global_dir).await.unwrap();
        assert_eq!(loaded.default_model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(loaded.compaction.unwrap().keep_recent_tokens, Some(5000));

        // Project override merges on load; reload reflects a changed file.
        tokio::fs::create_dir_all(project.join(".pi"))
            .await
            .unwrap();
        tokio::fs::write(
            Settings::project_path(&project),
            r#"{"default_model": "gpt-5"}"#,
        )
        .await
        .unwrap();
        let merged = Settings::load_at(&project, &global_dir).await.unwrap();
        assert_eq!(merged.default_model.as_deref(), Some("gpt-5"));
        assert_eq!(merged.compaction.unwrap().keep_recent_tokens, Some(5000));

        tokio::fs::write(
            Settings::project_path(&project),
            r#"{"default_model": "claude-opus-4-8"}"#,
        )
        .await
        .unwrap();
        let mut reloaded = Settings::default();
        reloaded.reload_at(&project, &global_dir).await.unwrap();
        assert_eq!(reloaded.default_model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn test_merge_project_compaction_lands_when_global_is_silent() {
        let mut global = Settings::default();
        let project = Settings::from_json(
            r#"{"compaction": {"enabled": false}, "show_cache_miss_notices": true}"#,
        )
        .unwrap();

        global.merge(&project);
        let resolved = global.resolved_compaction();
        assert!(!resolved.enabled);
        // Unmentioned fields fall back to the defaults.
        assert_eq!(resolved.reserve_tokens, 16384);
        assert_eq!(global.show_cache_miss_notices, Some(true));
    }
}
