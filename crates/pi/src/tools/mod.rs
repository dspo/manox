// Built-in tools — read, write, edit, bash, grep, find, ls.
//
// Each tool implements the AgentTool trait and operates through the
// ExecutionEnv abstraction, making them runtime-agnostic.

pub mod read;
pub mod write;
pub mod edit;
pub mod bash;
pub mod grep;
pub mod find;
pub mod ls;
pub mod file_mutation_queue;
pub mod output_accumulator;
pub mod path_utils;
pub mod truncate;

use crate::tool::AgentTool;
use std::collections::HashMap;

/// A registry of tools, keyed by name.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn AgentTool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        ToolRegistry {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn AgentTool>) {
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
        self.register(Box::new(read::ReadTool));
        self.register(Box::new(write::WriteTool));
        self.register(Box::new(edit::EditTool));
        self.register(Box::new(bash::BashTool::new(None)));
        self.register(Box::new(grep::GrepTool));
        self.register(Box::new(find::FindTool));
        self.register(Box::new(ls::LsTool));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_registry() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(read::ReadTool));
        assert_eq!(registry.names(), vec!["read"]);
        assert!(registry.get("read").is_some());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_register_defaults() {
        let mut registry = ToolRegistry::new();
        registry.register_defaults();
        let names = registry.names();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"write"));
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"find"));
        assert!(names.contains(&"ls"));
        assert_eq!(names.len(), 7);
    }
}