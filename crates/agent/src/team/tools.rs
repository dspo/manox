//! Team tools: shared coordination primitives available to the leader and
//! every worker member.
//!
//! All team tools are stateless — they read the owning thread's team (via
//! [`ToolContext::team`]) and label (via [`ToolContext::agent_label`]) at call
//! time, so the same tool instances serve the leader and all members. They run
//! synchronously (entity updates are in-memory) and return [`Task::ready`];
//! no background work, no `&dyn ToolContext` crossing an await.
//!
//! `requires_approval` is `false` throughout: creating a task or sending a
//! peer message is metadata, not a world-mutating action. Member tool calls
//! that DO mutate the world (write_file, bash, …) still bubble their own
//! authorizations up to the leader via the team's auth subscription.

use std::sync::Arc;

use gpui::{App, Task};
use schemars::JsonSchema;
use serde::{Deserialize, de};
use tokio_util::sync::CancellationToken;

use crate::team::Team;
use crate::tool::{AgentTool, AnyAgentTool, ToolContext};

use super::task_list::TaskStatus;

/// Tri-state owner deserializer. `Option<Option<String>>` as written collapses
/// JSON `null` to the outer `None` (serde's `Option` Visitor short-circuits
/// `null`), erasing the unassign intent — so the field is parsed by hand:
/// absent → `None` (leave unchanged), `null` → `Some(None)` (unassign back to
/// the pool), `"x"` → `Some(Some("x"))` (assign). `#[serde(default)]` covers
/// the absent case; this covers the present cases.
fn deserialize_owner<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Null => Ok(Some(None)),
        serde_json::Value::String(s) => Ok(Some(Some(s))),
        other => Err(de::Error::custom(format!(
            "owner must be a string or null, got {other}"
        ))),
    }
}

// ─── helpers ──────────────────────────────────────────────────────────────

/// The owning thread's team, or an `Err` tool-result string naming the remedy.
/// Cloning the `Entity` is an `Arc` bump — cheap, and it lets the tool update
/// the team outside any borrow of `cx`.
fn team_or_err(ctx: &dyn ToolContext) -> Result<gpui::Entity<Team>, String> {
    ctx.team()
        .cloned()
        .ok_or_else(|| "no active team: create one with team_create first".to_string())
}

// ─── task_create ──────────────────────────────────────────────────────────

pub struct TaskCreateTool;

#[derive(Deserialize, JsonSchema)]
struct TaskCreateInput {
    /// One-line task title. The model uses this as the coordination handle.
    subject: String,
    /// Optional longer description / acceptance notes. Omit for a simple task.
    #[serde(default)]
    description: Option<String>,
}

impl AgentTool for TaskCreateTool {
    fn name(&self) -> &str {
        "task_create"
    }
    fn description(&self) -> &str {
        "Create a task on the shared team task list. Returns the new task id (e.g. T1). \
         Use for the team's coordination board — the leader breaks work into tasks, members \
         claim and update them. Requires an active team (team_create)."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<TaskCreateInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<TaskCreateInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let team = match team_or_err(ctx) {
            Ok(t) => t,
            Err(e) => return Task::ready(Err(e)),
        };
        let tasks = team.read(cx).tasks().clone();
        let id = tasks.update(cx, |l, cx| l.create(parsed.subject, parsed.description, cx));
        Task::ready(Ok(format!("created task {id}")))
    }
}

// ─── task_list ────────────────────────────────────────────────────────────

pub struct TaskListTool;

#[derive(Deserialize, JsonSchema)]
struct TaskListInput {}

impl AgentTool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }
    fn description(&self) -> &str {
        "List all tasks on the shared team task list (id, status, owner, subject). \
         Read-only snapshot of the coordination board."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<TaskListInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(_) = serde_json::from_value::<TaskListInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let team = match team_or_err(ctx) {
            Ok(t) => t,
            Err(e) => return Task::ready(Err(e)),
        };
        let tasks = team.read(cx).tasks().clone();
        let snapshot: Vec<crate::team::Task> = tasks.update(cx, |l, _cx| l.tasks().to_vec());
        if snapshot.is_empty() {
            return Task::ready(Ok("(no tasks)".to_string()));
        }
        let mut lines = String::new();
        for t in &snapshot {
            let owner = t.owner.as_deref().unwrap_or("—");
            lines.push_str(&format!(
                "{} [{}] {} — {}\n",
                t.id, t.status, owner, t.subject
            ));
        }
        Task::ready(Ok(lines.trim_end().to_string()))
    }
}

// ─── task_update ──────────────────────────────────────────────────────────

pub struct TaskUpdateTool;

#[derive(Deserialize, JsonSchema)]
struct TaskUpdateInput {
    /// Id of the task to update (e.g. T1).
    id: String,
    /// New status. Omit to leave unchanged.
    #[serde(default)]
    status: Option<TaskStatus>,
    /// New owner (a member name). Tri-state: omit the field to leave unchanged;
    /// pass `null` to unassign back to the pool; pass a name to assign.
    #[serde(default, deserialize_with = "deserialize_owner")]
    owner: Option<Option<String>>,
    /// New subject. Omit to leave unchanged.
    #[serde(default)]
    subject: Option<String>,
}

impl AgentTool for TaskUpdateTool {
    fn name(&self) -> &str {
        "task_update"
    }
    fn description(&self) -> &str {
        "Update a task's status, owner, or subject. Each field is optional — omit a \
         field to leave it unchanged; for `owner`, pass null to unassign. Returns the \
         updated task or an error if the id is unknown."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<TaskUpdateInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<TaskUpdateInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let team = match team_or_err(ctx) {
            Ok(t) => t,
            Err(e) => return Task::ready(Err(e)),
        };
        let tasks = team.read(cx).tasks().clone();
        let result = tasks.update(cx, |l, cx| {
            l.update(&parsed.id, parsed.status, parsed.owner, parsed.subject, cx)
        });
        match result {
            Ok(()) => {
                let task = tasks.read(cx).get(&parsed.id).cloned();
                match task {
                    Some(t) => {
                        let owner = t.owner.as_deref().unwrap_or("—");
                        Task::ready(Ok(format!(
                            "{} [{}] {} — {}",
                            t.id, t.status, owner, t.subject
                        )))
                    }
                    None => Task::ready(Ok(format!("updated task {}", parsed.id))),
                }
            }
            Err(e) => Task::ready(Err(e)),
        }
    }
}

// ─── task_get ─────────────────────────────────────────────────────────────

pub struct TaskGetTool;

#[derive(Deserialize, JsonSchema)]
struct TaskGetInput {
    /// Task id (e.g. T1).
    id: String,
}

impl AgentTool for TaskGetTool {
    fn name(&self) -> &str {
        "task_get"
    }
    fn description(&self) -> &str {
        "Read a single task by id (full subject, description, status, owner)."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<TaskGetInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<TaskGetInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let team = match team_or_err(ctx) {
            Ok(t) => t,
            Err(e) => return Task::ready(Err(e)),
        };
        let tasks = team.read(cx).tasks().clone();
        match tasks.read(cx).get(&parsed.id) {
            Some(t) => {
                let owner = t.owner.as_deref().unwrap_or("(unassigned)");
                let desc = t.description.as_deref().unwrap_or("(no description)");
                Task::ready(Ok(format!(
                    "{} [{}] owner={}\nsubject: {}\ndescription: {}",
                    t.id, t.status, owner, t.subject, desc
                )))
            }
            None => Task::ready(Err(format!("task {} not found", parsed.id))),
        }
    }
}

// ─── send_message ─────────────────────────────────────────────────────────

pub struct SendMessageTool;

#[derive(Deserialize, JsonSchema)]
struct SendMessageInput {
    /// Recipient: a member name, "lead" (the leader / main thread), or "all"
    /// (broadcast to leader + every member except yourself).
    to: String,
    /// Message body. Delivered as a user-role message to the recipient's
    /// conversation, prefixed `[from {you}]`, so the recipient model sees it.
    message: String,
}

impl AgentTool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }
    fn description(&self) -> &str {
        "Send a peer message to a teammate (\"lead\", a member name, or \"all\" for \
         broadcast). An idle recipient processes it immediately; a busy recipient's \
         message queues and flushes when its current turn ends. Use to hand off work, \
         report progress, or ask a clarifying question mid-flight."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<SendMessageInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<SendMessageInput>(input) else {
            return Task::ready(Err("input parse failed".to_string()));
        };
        let team = match team_or_err(ctx) {
            Ok(t) => t,
            Err(e) => return Task::ready(Err(e)),
        };
        let from = ctx.agent_label().to_string();
        let result = team.update(cx, |t, cx| t.deliver(&from, &parsed.to, parsed.message, cx));
        match result {
            Ok(()) => Task::ready(Ok(format!("sent to {}", parsed.to))),
            Err(e) => Task::ready(Err(e)),
        }
    }
}

// ─── registry assembly ────────────────────────────────────────────────────

/// The shared team tools, registered for both the leader (in `main_registry`)
/// and every worker member (in `build_member_registry`). These are the
/// coordination primitives — `task_*` and `send_message`. Team-management
/// tools (`team_create` / `team_spawn` / `team_disband`) are leader-only and
/// appended separately.
pub fn shared_tools() -> Vec<AnyAgentTool> {
    vec![
        Arc::new(TaskCreateTool) as AnyAgentTool,
        Arc::new(TaskListTool) as AnyAgentTool,
        Arc::new(TaskUpdateTool) as AnyAgentTool,
        Arc::new(TaskGetTool) as AnyAgentTool,
        Arc::new(SendMessageTool) as AnyAgentTool,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ThreadRecord;
    use crate::team::{Member, Team};
    use crate::thread::Thread;
    use gpui::TestAppContext;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn team_with_leader(cx: &mut gpui::App) -> (gpui::Entity<Team>, gpui::Entity<Thread>) {
        let leader = Thread::restore(ThreadRecord::for_test("lead", "/tmp", Vec::new()), None, cx);
        let team = Team::new("squad".into(), leader.downgrade(), cx);
        leader.update(cx, |t, cx| t.set_team(team.clone(), cx));
        (team, leader)
    }

    /// Drive a synchronous tool to completion: build the `Task` (a `Task::ready`
    /// for these tools), then park the executor to await it. Mirrors the
    /// self_info test harness. The `Task` is `'static`, so it moves into the
    /// spawned future with no reference capture.
    fn run_tool(
        cx: &mut TestAppContext,
        tool: &dyn AgentTool,
        input: serde_json::Value,
        ctx: &dyn ToolContext,
    ) -> Result<String, String> {
        let task = cx.update(|cx| tool.run(input, CancellationToken::new(), ctx, cx));
        let out: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
        let r = out.clone();
        cx.spawn(|_cx| async move {
            *r.lock().unwrap() = Some(task.await);
        })
        .detach();
        cx.run_until_parked();
        out.lock()
            .unwrap()
            .take()
            .expect("tool task did not complete")
    }

    fn snapshot(
        cx: &TestAppContext,
        thread: &gpui::Entity<Thread>,
    ) -> crate::tool::ToolContextSnapshot {
        thread.read_with(cx, |t, _| crate::tool::ToolContextSnapshot::from_thread(t))
    }

    #[test]
    fn task_tools_crud_via_tool_run() {
        crate::agent_def::init();
        let mut cx = TestAppContext::single();
        let (_team, leader) = cx.update(team_with_leader);
        let ctx = snapshot(&cx, &leader);

        // create T1
        let id = run_tool(
            &mut cx,
            &TaskCreateTool,
            serde_json::json!({"subject":"wire UI","description":"tabs"}),
            &ctx,
        )
        .unwrap();
        assert!(id.contains("T1"), "got: {id}");

        // list
        let list = run_tool(&mut cx, &TaskListTool, serde_json::json!({}), &ctx).unwrap();
        assert!(list.contains("wire UI"), "got: {list}");

        // update: assign + in_progress
        let upd = run_tool(
            &mut cx,
            &TaskUpdateTool,
            serde_json::json!({"id":"T1","status":"in_progress","owner":"plan"}),
            &ctx,
        )
        .unwrap();
        assert!(upd.contains("in_progress"), "got: {upd}");
        assert!(upd.contains("plan"), "got: {upd}");

        // get
        let got = run_tool(&mut cx, &TaskGetTool, serde_json::json!({"id":"T1"}), &ctx).unwrap();
        assert!(got.contains("tabs"), "description present: {got}");

        // unassign via owner: null
        let _ = run_tool(
            &mut cx,
            &TaskUpdateTool,
            serde_json::json!({"id":"T1","owner":null}),
            &ctx,
        )
        .unwrap();
        let got = run_tool(&mut cx, &TaskGetTool, serde_json::json!({"id":"T1"}), &ctx).unwrap();
        assert!(got.contains("unassigned"), "owner cleared: {got}");

        // unknown id
        let err = run_tool(
            &mut cx,
            &TaskUpdateTool,
            serde_json::json!({"id":"T9","status":"completed"}),
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("T9"), "got: {err}");
    }

    #[test]
    fn tools_error_without_team() {
        crate::agent_def::init();
        let mut cx = TestAppContext::single();
        // Bare thread, no team attached.
        let thread = cx.update(|cx| {
            Thread::restore(ThreadRecord::for_test("solo", "/tmp", Vec::new()), None, cx)
        });
        let ctx = snapshot(&cx, &thread);
        let err = run_tool(
            &mut cx,
            &TaskCreateTool,
            serde_json::json!({"subject":"x"}),
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("no active team"), "got: {err}");
        let err = run_tool(
            &mut cx,
            &SendMessageTool,
            serde_json::json!({"to":"lead","message":"hi"}),
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("no active team"), "got: {err}");
    }

    #[test]
    fn send_message_delivers_to_idle_member() {
        crate::agent_def::init();
        let mut cx = TestAppContext::single();
        let (team, leader) = cx.update(team_with_leader);
        // Add a member thread (bare, no model — run_turn no-ops).
        let member = cx.update(|cx| {
            Thread::restore(ThreadRecord::for_test("plan", "/tmp", Vec::new()), None, cx)
        });
        cx.update(|cx| {
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "explorer".into(), member.downgrade()),
                    cx,
                )
            })
        })
        .unwrap();
        // Attach the team to the member so the recipient's own `agent_label`
        // and `from` are coherent if it ever sends a reply.
        cx.update(|cx| member.update(cx, |m, cx| m.set_team(team.clone(), cx)));

        let ctx = snapshot(&cx, &leader);
        let _ = run_tool(
            &mut cx,
            &SendMessageTool,
            serde_json::json!({"to":"plan","message":"go"}),
            &ctx,
        )
        .unwrap();
        cx.run_until_parked();
        let msgs = member.read_with(&cx, |t, _| t.messages().to_vec());
        assert_eq!(msgs.len(), 1, "member received the message");
    }

    #[test]
    fn shared_tools_names_are_unique() {
        let tools = shared_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate tool names: {names:?}");
        for expected in [
            "task_create",
            "task_list",
            "task_update",
            "task_get",
            "send_message",
        ] {
            assert!(names.contains(&expected), "missing {expected}: {names:?}");
        }
    }
}
