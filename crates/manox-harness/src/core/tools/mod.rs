// Built-in tools — read, write, edit, bash, grep, glob, ls.
//
// Each tool implements the AgentTool trait and operates through the
// ExecutionEnv abstraction, making them runtime-agnostic.

pub mod bash;
pub mod edit;
pub mod edit_diff;
pub mod file_mutation_queue;
pub mod glob;
pub mod grep;
pub mod ls;
pub mod path_utils;
pub mod read;
pub mod truncate;
pub mod write;

use crate::tool::AgentTool;
use std::collections::HashMap;
use std::sync::Arc;

/// A registry of tools, keyed by name.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn AgentTool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        ToolRegistry {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn AgentTool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn AgentTool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(|k| k.as_str()).collect()
    }

    pub fn all(&self) -> Vec<&dyn AgentTool> {
        self.tools.values().map(|t| t.as_ref()).collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    /// Register all default built-in tools.
    pub fn register_defaults(&mut self) {
        self.register(Arc::new(read::ReadTool));
        self.register(Arc::new(write::WriteTool));
        self.register(Arc::new(edit::EditTool::default()));
        self.register(Arc::new(bash::BashTool::new(None)));
        self.register(Arc::new(grep::GrepTool));
        self.register(Arc::new(glob::GlobTool));
        self.register(Arc::new(ls::LsTool));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_registry() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(read::ReadTool));
        assert_eq!(registry.names(), vec!["Read"]);
        assert!(registry.get("Read").is_some());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_register_defaults() {
        let mut registry = ToolRegistry::new();
        registry.register_defaults();
        let names = registry.names();
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Write"));
        assert!(names.contains(&"Edit"));
        assert!(names.contains(&"Bash"));
        assert!(names.contains(&"Grep"));
        assert!(names.contains(&"Glob"));
        assert!(!names.contains(&"Find"));
        assert!(names.contains(&"Ls"));
        assert_eq!(names.len(), 7);
    }
}
