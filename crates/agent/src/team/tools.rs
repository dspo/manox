//! Task tools (TaskCreate/TaskList/TaskUpdate/TaskGet).
//!
//! Operate on `Arc<Mutex<PlainTaskList>>` (bus-owned, tokio-safe) directly
//! — no `BackendNotice` round-trip, no gpui `Entity`.

use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::team::task_list::{Task, TaskStatus};

/// A plain (non-gpui) task list — tokio-safe, no Entity/EventEmitter.
/// Owned by `AgentBus` as `Arc<Mutex<PlainTaskList>>`; Task* tools call
/// its methods directly (no round-trip to the facade).
pub struct PlainTaskList {
    tasks: Vec<Task>,
    next_seq: u64,
}

impl PlainTaskList {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            next_seq: 0,
        }
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn create(&mut self, subject: String, description: Option<String>) -> String {
        self.next_seq += 1;
        let id = format!("T{}", self.next_seq);
        self.tasks.push(Task {
            id: id.clone(),
            subject,
            description,
            status: TaskStatus::Pending,
            owner: None,
        });
        id
    }

    pub fn update(
        &mut self,
        id: &str,
        status: Option<TaskStatus>,
        owner: Option<Option<String>>,
        subject: Option<String>,
    ) -> Result<(), String> {
        let task = self
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("task {id} not found"))?;
        if let Some(s) = status {
            task.status = s;
        }
        if let Some(o) = owner {
            task.owner = o;
        }
        if let Some(s) = subject {
            task.subject = s;
        }
        Ok(())
    }
}

impl Default for PlainTaskList {
    fn default() -> Self {
        Self::new()
    }
}

// ── Input schemas ─────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TaskCreateInput {
    subject: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TaskListInput {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TaskUpdateInput {
    id: String,
    #[serde(default)]
    status: Option<TaskStatus>,
    #[serde(default)]
    owner: Option<Option<String>>,
    #[serde(default)]
    subject: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TaskGetInput {
    id: String,
}

fn schema<T: JsonSchema>() -> serde_json::Value {
    let mut value = serde_json::to_value(schemars::schema_for!(T)).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("$schema");
        obj.remove("$defs");
    }
    value
}

// ── Tools ────────────────────────────────────────────────────────────────

pub struct TaskCreateTool {
    list: Arc<Mutex<PlainTaskList>>,
}

impl TaskCreateTool {
    pub fn new(_notice_tx: mpsc::UnboundedSender<crate::thread_engine::BackendNotice>) -> Self {
        Self {
            list: Arc::new(Mutex::new(PlainTaskList::new())),
        }
    }

    pub fn with_list(list: Arc<Mutex<PlainTaskList>>) -> Self {
        Self { list }
    }
}

#[async_trait::async_trait]
impl AgentTool for TaskCreateTool {
    fn name(&self) -> &str {
        "TaskCreate"
    }
    fn description(&self) -> &str {
        "Add a task to the shared task list (starts `pending`, unassigned). \
         Returns the new task id."
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn requires_approval(&self, _params: &serde_json::Value) -> bool {
        false
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TaskCreateInput>()
    }
    async fn execute(
        &self,
        _id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: TaskCreateInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let mut list = self.list.lock().unwrap();
        let id = list.create(input.subject, input.description);
        Ok(AgentToolResult::text(format!("created task {id}")))
    }
}

pub struct TaskListTool {
    list: Arc<Mutex<PlainTaskList>>,
}

impl TaskListTool {
    pub fn new(_notice_tx: mpsc::UnboundedSender<crate::thread_engine::BackendNotice>) -> Self {
        Self {
            list: Arc::new(Mutex::new(PlainTaskList::new())),
        }
    }

    pub fn with_list(list: Arc<Mutex<PlainTaskList>>) -> Self {
        Self { list }
    }
}

#[async_trait::async_trait]
impl AgentTool for TaskListTool {
    fn name(&self) -> &str {
        "TaskList"
    }
    fn description(&self) -> &str {
        "List all tasks on the shared task list with id, subject, status, and owner."
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn requires_approval(&self, _params: &serde_json::Value) -> bool {
        false
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TaskListInput>()
    }
    async fn execute(
        &self,
        _id: &str,
        _params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let list = self.list.lock().unwrap();
        if list.tasks().is_empty() {
            return Ok(AgentToolResult::text("task list is empty"));
        }
        let rendered = list
            .tasks()
            .iter()
            .map(|t| {
                format!(
                    "{} [{}] {} (owner: {})",
                    t.id,
                    t.status,
                    t.subject,
                    t.owner.as_deref().unwrap_or("unassigned")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(AgentToolResult::text(rendered))
    }
}

pub struct TaskUpdateTool {
    list: Arc<Mutex<PlainTaskList>>,
}

impl TaskUpdateTool {
    pub fn new(_notice_tx: mpsc::UnboundedSender<crate::thread_engine::BackendNotice>) -> Self {
        Self {
            list: Arc::new(Mutex::new(PlainTaskList::new())),
        }
    }

    pub fn with_list(list: Arc<Mutex<PlainTaskList>>) -> Self {
        Self { list }
    }
}

#[async_trait::async_trait]
impl AgentTool for TaskUpdateTool {
    fn name(&self) -> &str {
        "TaskUpdate"
    }
    fn description(&self) -> &str {
        "Update a task on the shared list: change `status` \
         (pending/in_progress/completed), assign/clear `owner` (a member \
         name or null), or edit `subject`. Omitted fields stay unchanged."
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn requires_approval(&self, _params: &serde_json::Value) -> bool {
        false
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TaskUpdateInput>()
    }
    async fn execute(
        &self,
        _id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: TaskUpdateInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let mut list = self.list.lock().unwrap();
        list.update(&input.id, input.status, input.owner, input.subject)
            .map_err(ToolError::ExecutionFailed)?;
        Ok(AgentToolResult::text(format!("updated task {}", input.id)))
    }
}

pub struct TaskGetTool {
    list: Arc<Mutex<PlainTaskList>>,
}

impl TaskGetTool {
    pub fn new(_notice_tx: mpsc::UnboundedSender<crate::thread_engine::BackendNotice>) -> Self {
        Self {
            list: Arc::new(Mutex::new(PlainTaskList::new())),
        }
    }

    pub fn with_list(list: Arc<Mutex<PlainTaskList>>) -> Self {
        Self { list }
    }
}

#[async_trait::async_trait]
impl AgentTool for TaskGetTool {
    fn name(&self) -> &str {
        "TaskGet"
    }
    fn description(&self) -> &str {
        "Get one task from the shared list by id, including its description."
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn requires_approval(&self, _params: &serde_json::Value) -> bool {
        false
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TaskGetInput>()
    }
    async fn execute(
        &self,
        _id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: TaskGetInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let list = self.list.lock().unwrap();
        let rendered = list
            .get(&input.id)
            .map(|t| {
                format!(
                    "{} [{}] {}\nowner: {}\ndescription: {}",
                    t.id,
                    t.status,
                    t.subject,
                    t.owner.as_deref().unwrap_or("unassigned"),
                    t.description.as_deref().unwrap_or("(none)")
                )
            })
            .ok_or_else(|| ToolError::ExecutionFailed(format!("task {} not found", input.id)))?;
        Ok(AgentToolResult::text(rendered))
    }
}
