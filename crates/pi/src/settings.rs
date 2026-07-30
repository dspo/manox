// Settings management with global/project scope merging.
//
// Settings are loaded from JSON files: global settings from ~/.pi/agent/settings.json
// and project-level overrides from .pi/settings.json in the project root.

use serde::{Deserialize, Serialize};

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
    /// Compaction settings.
    #[serde(default)]
    pub compaction: crate::compaction::CompactionSettings,
    /// Whether to show cache miss notices.
    #[serde(default)]
    pub show_cache_miss_notices: bool,
    /// Shell command prefix (e.g. "sudo").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_command_prefix: Option<String>,
    /// External editor path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_editor: Option<String>,
}

impl Settings {
    /// Merge project-level overrides into global settings.
    /// Project settings take precedence for non-None values.
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
        // Compaction is always overwritten if explicitly set
        self.compaction = project.compaction.clone();
        self.show_cache_miss_notices = project.show_cache_miss_notices;
    }

    /// Load settings from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Serialize settings to a JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
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
        assert!(settings.compaction.enabled);
    }
}
