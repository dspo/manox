use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::cx_agent::approval::ToolCategory;
use crate::cx_agent::provider::CxToolDefinition;

mod bash;
mod edit_file;
mod glob;
mod grep;
mod read_file;
mod write_file;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInvocation {
    pub name: String,
    pub arguments: Value,
}

#[allow(dead_code)]
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn category(&self) -> ToolCategory;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;

    async fn invoke(&self, arguments: Value) -> Result<String>;

    fn definition(&self) -> CxToolDefinition {
        CxToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

#[derive(Default)]
pub struct Registry {
    order: Vec<String>,
    tools: HashMap<String, Box<dyn Tool>>,
}

impl Registry {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)]
    pub fn with_builtins() -> Result<Self> {
        let mut registry = Self::new();
        registry.register_builtin_tools()?;
        Ok(registry)
    }

    #[allow(dead_code)]
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<()> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(anyhow!("tool {name} is already registered"));
        }
        self.order.push(name.clone());
        self.tools.insert(name, tool);
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|tool| tool.as_ref())
    }

    #[allow(dead_code)]
    pub fn definitions(&self) -> Vec<CxToolDefinition> {
        self.order
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|tool| tool.definition())
            .collect()
    }

    #[allow(dead_code)]
    pub async fn invoke(&self, name: &str, arguments: Value) -> Result<String> {
        let tool = self
            .get(name)
            .ok_or_else(|| anyhow!("unknown tool {name}; available tools: {}", self.available()))?;
        tool.invoke(arguments).await
    }

    fn register_builtin_tools(&mut self) -> Result<()> {
        self.register(Box::new(read_file::ReadFileTool))?;
        self.register(Box::new(write_file::WriteFileTool))?;
        self.register(Box::new(edit_file::EditFileTool))?;
        self.register(Box::new(bash::BashTool))?;
        self.register(Box::new(grep::GrepTool))?;
        self.register(Box::new(glob::GlobTool))?;
        Ok(())
    }

    fn available(&self) -> String {
        self.order.join(", ")
    }
}

pub(super) fn parse_args<T>(tool_name: &str, arguments: Value) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(arguments)
        .map_err(|err| anyhow!("{tool_name} received invalid arguments: {err}"))
}

pub(super) fn validate_path(tool_name: &str, raw_path: &str) -> Result<PathBuf> {
    if raw_path.trim().is_empty() {
        return Err(anyhow!("{tool_name} path must not be empty"));
    }
    Ok(PathBuf::from(raw_path))
}

pub(super) fn display_path(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
pub(super) mod test_support {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    pub struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        pub fn new(label: &str) -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::current_dir()
                .expect("current dir")
                .join("target")
                .join("cx-agent-tool-tests")
                .join(format!("{label}-{id}"));
            if path.exists() {
                fs::remove_dir_all(&path).expect("remove old test dir");
            }
            fs::create_dir_all(&path).expect("create test dir");
            Self { path }
        }

        pub fn path(&self) -> &Path {
            &self.path
        }

        pub fn file_path(&self, relative: &str) -> PathBuf {
            self.path.join(relative)
        }

        pub fn write(&self, relative: &str, content: &str) -> PathBuf {
            let path = self.file_path(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent dirs");
            }
            fs::write(&path, content).expect("write test file");
            path
        }

        pub fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.file_path(relative)).expect("read test file")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Registry;

    #[tokio::test(flavor = "current_thread")]
    async fn registry_exposes_builtins_and_invokes_by_name() {
        let registry = Registry::with_builtins().expect("registry");
        let names: Vec<String> = registry
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();

        assert_eq!(
            names,
            vec![
                "read_file",
                "write_file",
                "edit_file",
                "bash",
                "grep",
                "glob",
            ]
        );

        let missing = registry
            .invoke("missing_tool", json!({}))
            .await
            .expect_err("missing tool should fail");
        assert!(
            missing
                .to_string()
                .contains("unknown tool missing_tool; available tools")
        );
    }
}
