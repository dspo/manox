//! Team tools for the pi path (ported from the retired manox harness).
//!
//! The tools run on tokio; all team state is gpui-side (`Entity<Team>` owned
//! by the leader facade). Every call rides the `BackendNotice::TeamRequest`
//! round trip: the tool posts the op with a responder channel, the facade
//! executes it on the gpui thread and replies with the model-facing string —
//! the same architecture as the approval gate and browser round trips.

use gpui::{Context, Entity};
use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::task_list::TaskStatus;
use super::{LEADER_NAME, Member, Team};
use crate::thread::Thread;
use crate::thread_engine::{BackendNotice, MemberSpec, TeamOp};

/// Send `op` to the facade and await the reply.
async fn team_round_trip(
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    op: TeamOp,
) -> Result<AgentToolResult, ToolError> {
    let (tx, rx) = async_channel::bounded(1);
    notice_tx
        .send(BackendNotice::TeamRequest { op, responder: tx })
        .map_err(|_| ToolError::ExecutionFailed("engine actor gone".into()))?;
    rx.recv()
        .await
        .map_err(|_| ToolError::ExecutionFailed("team request dropped".into()))?
        .map(AgentToolResult::text)
        .map_err(ToolError::ExecutionFailed)
}

fn schema<T: JsonSchema>() -> serde_json::Value {
    let mut value = serde_json::to_value(schemars::schema_for!(T)).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("$schema");
        obj.remove("$defs");
    }
    value
}

// ─── inputs ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
pub struct MemberSpecInput {
    /// Worker name (unique within the team; used as the routing handle for
    /// `SendMessage` and the auth-bubble composite id).
    pub name: String,
    /// Short role label shown in the roster UI (e.g. "explorer").
    pub role: String,
    /// Sub-agent definition the member's persona derives from (kept for
    /// contract parity; the pi member runs as a full pi session — see PR
    /// assumptions).
    #[serde(default)]
    pub subagent_type: Option<String>,
    /// The member's first task. Becomes its opening user message; the member
    /// has no access to the leader's conversation, so include any needed
    /// file paths, error text, or context here.
    pub prompt: String,
}

impl MemberSpecInput {
    fn into_spec(self) -> MemberSpec {
        MemberSpec {
            name: self.name,
            role: self.role,
            prompt: self.prompt,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TeamCreateInput {
    /// Team name (display only).
    name: String,
    /// Initial roster of worker members to spawn alongside the team. Omit
    /// for an empty team you grow later with `TeamSpawn`.
    #[serde(default)]
    members: Vec<MemberSpecInput>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendMessageInput {
    /// Recipient: a member name, `lead` for the leader, or `all` to
    /// broadcast to everyone except the sender.
    to: String,
    /// Message body.
    content: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TaskCreateInput {
    /// One-line task summary (imperative).
    subject: String,
    /// Optional longer description / acceptance criteria.
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TaskListInput {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TaskUpdateInput {
    /// Task id (`T1`, `T2`, …).
    id: String,
    /// New status; omit to leave unchanged.
    #[serde(default)]
    status: Option<TaskStatus>,
    /// Assignee: omit to leave unchanged, `null` to unassign, or a member
    /// name.
    #[serde(default)]
    owner: Option<Option<String>>,
    /// New subject; omit to leave unchanged.
    #[serde(default)]
    subject: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TaskGetInput {
    /// Task id (`T1`, `T2`, …).
    id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

// ─── tools ──────────────────────────────────────────────────────────────────

macro_rules! team_tool_ctor {
    ($tool:ident) => {
        pub struct $tool {
            notice_tx: mpsc::UnboundedSender<BackendNotice>,
        }
        impl $tool {
            pub fn new(notice_tx: mpsc::UnboundedSender<BackendNotice>) -> Self {
                Self { notice_tx }
            }
        }
    };
}

team_tool_ctor!(TeamCreateTool);
team_tool_ctor!(TeamSpawnTool);
team_tool_ctor!(TeamDisbandTool);
team_tool_ctor!(SendMessageTool);
team_tool_ctor!(TaskCreateTool);
team_tool_ctor!(TaskListTool);
team_tool_ctor!(TaskUpdateTool);
team_tool_ctor!(TaskGetTool);

#[async_trait::async_trait]
impl AgentTool for TeamCreateTool {
    fn name(&self) -> &str {
        "TeamCreate"
    }
    fn description(&self) -> &str {
        "Form a peer-agents team with you (the main agent) as leader and the \
         listed sub-agents as long-lived worker members. Members coordinate via \
         the shared task list and `SendMessage`; each member runs autonomously \
         to completion and reports back. Use for parallel sub-tasks that need to \
         coordinate or share progress — NOT for independent fire-and-forget work \
         (use the `Agent` tool for that). Only one team may be active at a time; \
         disband with `TeamDisband` before forming another. Assign members \
         disjoint write ranges to avoid write contention."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TeamCreateInput>()
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: TeamCreateInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        team_round_trip(
            &self.notice_tx,
            TeamOp::Create {
                name: input.name,
                members: input.members.into_iter().map(|m| m.into_spec()).collect(),
            },
        )
        .await
    }
}

#[async_trait::async_trait]
impl AgentTool for TeamSpawnTool {
    fn name(&self) -> &str {
        "TeamSpawn"
    }
    fn description(&self) -> &str {
        "Add a worker member to the active team. The team must already exist \
         (TeamCreate). The new member runs autonomously and reports back via \
         `SendMessage`. Refused if the roster is full (5 workers max)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<MemberSpecInput>()
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: MemberSpecInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        team_round_trip(
            &self.notice_tx,
            TeamOp::Spawn {
                spec: input.into_spec(),
            },
        )
        .await
    }
}

#[async_trait::async_trait]
impl AgentTool for TeamDisbandTool {
    fn name(&self) -> &str {
        "TeamDisband"
    }
    fn description(&self) -> &str {
        "Disband the active team: drop all worker member threads, release the \
         shared task list, and clear the leader's team. No-op message if no team \
         is active. Member conversations are session-scoped and not persisted."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<EmptyInput>()
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let _input: EmptyInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        team_round_trip(&self.notice_tx, TeamOp::Disband).await
    }
}

#[async_trait::async_trait]
impl AgentTool for SendMessageTool {
    fn name(&self) -> &str {
        "SendMessage"
    }
    fn description(&self) -> &str {
        "Send a message to a team member (by name), the leader (`lead`), or \
         `all` (broadcast to everyone except you). Delivery to an idle \
         recipient triggers their turn immediately; a busy recipient receives \
         it when their current turn ends. Use this to report progress, ask \
         questions, and coordinate."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<SendMessageInput>()
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: SendMessageInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        team_round_trip(
            &self.notice_tx,
            TeamOp::Send {
                to: input.to,
                content: input.content,
            },
        )
        .await
    }
}

#[async_trait::async_trait]
impl AgentTool for TaskCreateTool {
    fn name(&self) -> &str {
        "TaskCreate"
    }
    fn description(&self) -> &str {
        "Add a task to the team's shared task list (starts `pending`, \
         unassigned). Returns the new task id (`T1`, `T2`, …). Use the shared \
         list to coordinate who works on what."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TaskCreateInput>()
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: TaskCreateInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        team_round_trip(
            &self.notice_tx,
            TeamOp::TaskCreate {
                subject: input.subject,
                description: input.description,
            },
        )
        .await
    }
}

#[async_trait::async_trait]
impl AgentTool for TaskListTool {
    fn name(&self) -> &str {
        "TaskList"
    }
    fn description(&self) -> &str {
        "List all tasks on the team's shared task list with id, subject, \
         status, and owner."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TaskListInput>()
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let _input: TaskListInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        team_round_trip(&self.notice_tx, TeamOp::TaskList).await
    }
}

#[async_trait::async_trait]
impl AgentTool for TaskUpdateTool {
    fn name(&self) -> &str {
        "TaskUpdate"
    }
    fn description(&self) -> &str {
        "Update a task on the shared list: change `status` \
         (pending/in_progress/completed), assign/clear `owner` (a member name \
         or null), or edit `subject`. Omitted fields stay unchanged."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TaskUpdateInput>()
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: TaskUpdateInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        team_round_trip(
            &self.notice_tx,
            TeamOp::TaskUpdate {
                id: input.id,
                status: input.status,
                owner: input.owner,
                subject: input.subject,
            },
        )
        .await
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
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TaskGetInput>()
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: TaskGetInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        team_round_trip(&self.notice_tx, TeamOp::TaskGet { id: input.id }).await
    }
}

// ─── facade-side execution ──────────────────────────────────────────────────

/// Execute one [`TeamOp`] on the gpui thread (facade `handle_notice`). `self_`
/// is the thread facade the tool belongs to — the leader when its team is
/// owned here, a member otherwise.
pub fn execute_team_op(
    this: &mut Thread,
    op: TeamOp,
    cx: &mut Context<Thread>,
) -> Result<String, String> {
    match op {
        TeamOp::Create { name, members } => op_create(this, name, members, cx),
        TeamOp::Spawn { spec } => op_spawn(this, spec, cx),
        TeamOp::Send { to, content } => op_send(this, to, content, cx),
        TeamOp::Disband => op_disband(this, cx),
        TeamOp::TaskCreate {
            subject,
            description,
        } => with_tasks(this, cx, |tasks, cx| {
            let id = tasks.update(cx, |t, cx| t.create(subject, description, cx));
            Ok(format!("created task {id}"))
        }),
        TeamOp::TaskList => with_tasks(this, cx, |tasks, cx| {
            let rendered = tasks.read_with(cx, |t, _| {
                if t.tasks().is_empty() {
                    return "task list is empty".to_string();
                }
                t.tasks()
                    .iter()
                    .map(|task| {
                        format!(
                            "{} [{}] {} (owner: {})",
                            task.id,
                            task.status,
                            task.subject,
                            task.owner.as_deref().unwrap_or("unassigned")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            Ok(rendered)
        }),
        TeamOp::TaskUpdate {
            id,
            status,
            owner,
            subject,
        } => with_tasks(this, cx, |tasks, cx| {
            tasks.update(cx, |t, cx| t.update(&id, status, owner, subject, cx))?;
            Ok(format!("updated task {id}"))
        }),
        TeamOp::TaskGet { id } => with_tasks(this, cx, |tasks, cx| {
            let rendered = tasks.read_with(cx, |t, _| {
                t.get(&id).map(|task| {
                    format!(
                        "{} [{}] {}\nowner: {}\ndescription: {}",
                        task.id,
                        task.status,
                        task.subject,
                        task.owner.as_deref().unwrap_or("unassigned"),
                        task.description.as_deref().unwrap_or("(none)")
                    )
                })
            });
            rendered.ok_or_else(|| format!("task {id} not found"))
        }),
    }
}

/// Reach the calling thread's team: the leader owns it on `team`; a member
/// holds it on its own `team` field (set at spawn).
fn team_of(this: &Thread) -> Option<&Entity<Team>> {
    this.team()
}

fn with_tasks<F>(this: &mut Thread, cx: &mut Context<Thread>, f: F) -> Result<String, String>
where
    F: FnOnce(&Entity<super::TaskList>, &mut Context<Thread>) -> Result<String, String>,
{
    let Some(team) = team_of(this).cloned() else {
        return Err("no active team".to_string());
    };
    let tasks = team.read_with(cx, |t, _| t.tasks().clone());
    f(&tasks, cx)
}

fn op_create(
    this: &mut Thread,
    name: String,
    members: Vec<MemberSpec>,
    cx: &mut Context<Thread>,
) -> Result<String, String> {
    // Single-active-team guard: the team↔member ownership cycle relies on at
    // most one live team; a second would dangle the first's members.
    if this.team().is_some() {
        return Err("a team is already active; disband it with TeamDisband first".to_string());
    }
    let leader = cx.entity();
    let team = Team::new(name.clone(), leader.downgrade(), cx);
    this.set_team(team.clone(), cx);

    // Wire the leader's terminal-Stop → flush leader inbox. The leader is
    // mid-turn when TeamCreate runs; this subscription fires on its
    // subsequent turn ends. The flush is parked in a spawned task so it
    // lands after the current notice handling unwinds (the retired harness
    // used `cx.defer` for the same ordering).
    let team_w = team.downgrade();
    let leader_sub = cx.subscribe(
        &leader,
        move |_this: &mut Thread,
              _leader,
              ev: &crate::thread::ThreadEvent,
              cx: &mut Context<Thread>| {
            use crate::language_model::StopReason;
            if let crate::thread::ThreadEvent::Stop(reason) = ev
                && !matches!(reason, StopReason::ToolUse)
            {
                let tw = team_w.clone();
                cx.spawn(
                    async move |_this: gpui::WeakEntity<Thread>, cx: &mut gpui::AsyncApp| {
                        if let Some(t) = tw.upgrade() {
                            t.update(cx, |tm, cx| tm.flush_inbox(LEADER_NAME, cx));
                        }
                    },
                )
                .detach();
            }
        },
    );
    team.update(cx, |t, _cx| t.set_leader_sub(leader_sub));

    // Spawn each member; a failure aborts: disband the partial team so the
    // caller can retry cleanly rather than landing on a half-built team.
    let mut spawned = Vec::new();
    for spec in members {
        let member_name = spec.name.clone();
        match spawn_member(this, &team, spec, cx) {
            Ok(()) => spawned.push(member_name),
            Err(e) => {
                team.update(cx, |t, cx| t.disband(cx));
                this.clear_team(cx);
                return Err(format!("spawn of '{member_name}' failed: {e}"));
            }
        }
    }
    Ok(format!(
        "team '{}' created with {} member(s){}",
        name,
        spawned.len(),
        if spawned.is_empty() {
            String::new()
        } else {
            format!(": {}", spawned.join(", "))
        }
    ))
}

fn op_spawn(
    this: &mut Thread,
    spec: MemberSpec,
    cx: &mut Context<Thread>,
) -> Result<String, String> {
    let Some(team) = team_of(this).cloned() else {
        return Err("no active team; create one with TeamCreate".to_string());
    };
    if !team.read_with(cx, |t, _| t.has_room()) {
        return Err("team is full (5 workers max)".to_string());
    }
    let member_name = spec.name.clone();
    spawn_member(this, &team, spec, cx)?;
    Ok(format!("spawned member '{member_name}'"))
}

fn op_send(
    this: &mut Thread,
    to: String,
    content: String,
    cx: &mut Context<Thread>,
) -> Result<String, String> {
    let Some(team) = team_of(this).cloned() else {
        return Err("no active team".to_string());
    };
    let from = if team.read_with(cx, |t, _| t.is_leader(&cx.entity())) {
        LEADER_NAME.to_string()
    } else {
        this.agent_label().to_string()
    };
    team.update(cx, |t, cx| t.deliver(&from, &to, content, cx))?;
    Ok(format!("sent to {to}"))
}

fn op_disband(this: &mut Thread, cx: &mut Context<Thread>) -> Result<String, String> {
    let Some(team) = team_of(this).cloned() else {
        return Ok("no active team".to_string());
    };
    team.update(cx, |t, cx| t.disband(cx));
    this.clear_team(cx);
    Ok("team disbanded".to_string())
}

/// Spawn a long-lived team worker: an independent pi `Entity<Thread>`
/// inheriting the leader's cwd / model / approval mode / reasoning effort,
/// labeled with the member name. The member's `team` back-reference is set
/// so its `Task*`/`SendMessage` calls reach the shared list + router; the
/// subscription bubbles its authorizations to the leader under a composite
/// id and flushes its inbox on turn end.
fn spawn_member(
    leader: &mut Thread,
    team: &Entity<Team>,
    spec: MemberSpec,
    cx: &mut Context<Thread>,
) -> Result<(), String> {
    let member = leader.new_team_member(spec.name.clone(), cx);
    member.update(cx, |t, cx| t.set_team(team.clone(), cx));

    let team_w = team.downgrade();
    let name = spec.name.clone();
    let sub = cx.subscribe(
        &member,
        move |this: &mut Thread,
              member_ent: Entity<Thread>,
              ev: &crate::thread::ThreadEvent,
              cx: &mut Context<Thread>| {
            use crate::language_model::StopReason;
            match ev {
                crate::thread::ThreadEvent::ToolCallAuthorization {
                    id,
                    tool_name,
                    summary,
                    input,
                } => {
                    // Bubble to the leader under a composite id; the verdict
                    // routes back through `resolve_child_auth`.
                    let composite = format!("{name}::{id}");
                    if let Some(team) = this.team().cloned() {
                        let member_w = member_ent.downgrade();
                        let child_id = id.clone();
                        let composite_reg = composite.clone();
                        team.update(cx, move |t, _| {
                            t.register_child_auth(composite_reg, member_w, child_id)
                        });
                    }
                    let tool_name = tool_name.clone();
                    let summary = format!("[{name}] {summary}");
                    let input = input.clone();
                    cx.emit(crate::thread::ThreadEvent::ToolCallAuthorization {
                        id: composite,
                        tool_name,
                        summary,
                        input,
                    });
                }
                crate::thread::ThreadEvent::Stop(reason)
                    if !matches!(reason, StopReason::ToolUse) =>
                {
                    let tw = team_w.clone();
                    let name = name.clone();
                    cx.spawn(
                        async move |_this: gpui::WeakEntity<Thread>, cx: &mut gpui::AsyncApp| {
                            if let Some(t) = tw.upgrade() {
                                t.update(cx, |t, cx| t.flush_inbox(&name, cx));
                            }
                        },
                    )
                    .detach();
                }
                _ => {}
            }
        },
    );

    let member_name = spec.name.clone();
    let role = spec.role.clone();
    team.update(cx, |t, cx| {
        t.insert_member(Member::new(member_name.clone(), role, member.clone()), cx)?;
        t.set_member_sub(member_name.clone(), sub);
        Ok::<(), String>(())
    })?;

    // Opening task: the member runs it immediately (fire-and-forget worker).
    let prompt = spec.prompt.clone();
    member.update(cx, |t, cx| {
        t.insert_user_message_with_ui_metadata(prompt, None, cx);
        t.run_turn(cx);
    });
    Ok(())
}
