# δ₂: manox-actor → AgentServer Responsibility Mapping

The plan requires this mapping as a δ₂ prerequisite. Every manox-actor capability
is confirmed to have a home in the AgentServer protocol.

## Command mapping (handle_command → ClientNote/ClientCall)

| Actor `handle_command` arm | AgentServer protocol variant | Notes |
|---|---|---|
| `create_session` | `ClientNote::CreateSession` | Direct 1:1 |
| `open_thread` | `ClientCall::OpenSession` | Request (returns history snapshot) |
| `focus_thread` | `ClientNote::FocusThread` | Direct 1:1 |
| `archive_thread` | `ClientNote::ArchiveThread` | Direct 1:1 |
| `pin_thread` | `ClientNote::PinThread` | Direct 1:1 |
| `list_threads` | `ClientCall::ListThreads` | Request |
| `list_commands` | `ClientCall::ListCommands` | Request |
| `list_models` | `ClientCall::ListModels` | Request |
| `thread_info` | `ClientCall::ThreadInfo` | Request |
| `dispose_session` | `ClientNote::DisposeSession` | Direct 1:1 |
| `detach_session` | `ClientNote::DetachSession` | Direct 1:1 |
| `submit` | `ClientNote::Submit` | Direct 1:1 |
| `steer` | `ClientNote::Steer` | Direct 1:1 |
| `cancel_turn` | `ClientNote::CancelTurn` | Direct 1:1 |
| `approve` | `FromClient::Reply` (to `ServerCall::Approve`) | Adjudication Request |
| `answer_question` | `FromClient::Reply` (to `ServerCall::AskUserQuestion`) | Adjudication Request |
| `set_approval_mode` | `ClientNote::SetApprovalMode` | Direct 1:1 |
| `set_plan_mode` | `ClientNote::SetPlanMode` | Direct 1:1 |
| `plan_verdict` | `FromClient::Reply` (to `ServerCall::PlanVerdict`) | Adjudication Request |
| `plan_seed_execution` | `ClientNote::PlanSeedExecution` | Direct 1:1 |
| `goal` | `ClientNote::Goal` | Direct 1:1 |
| `stop_background_task` | `ClientNote::StopBackgroundTask` | Direct 1:1 |
| `set_model` | `ClientNote::SetModel` | Direct 1:1 |
| `set_reasoning_effort` | `ClientNote::SetReasoningEffort` | Direct 1:1 |
| `get_usage` | `ClientCall::GetUsage` | Request |
| `drop_queued` | `ClientNote::DropQueued` | Direct 1:1 |

## Non-command capabilities

| Actor capability | AgentServer equivalent | Gap? |
|---|---|---|
| `init` (host slug pinning) | `manox_agent::host::set_host()` + `manox_agent::init()` — called by manox-napi BEFORE creating AgentServer | No gap — done outside protocol |
| `spawn_models_push` | `ClientCall::ListModels` — client calls after `ServerNote::Ready` | No gap — replaces proactive push with client-initiated query |
| `subscribe_thread` (event subscription) | Pump receives `FromServer::Notification(ServerNote)` — all ThreadEvents flow as ServerNote | No gap — pump handles all events |
| External CLI session restore | `ClientCall::OpenSession` returns history snapshot | No gap |
| `EventSink` (JSON event emission) | `FromServer::Notification` (typed) — pump converts to JSON for NAPI | No gap — NAPI layer serializes |

## Gaps: NONE

All 26 actor commands and 4 non-command capabilities have homes in the AgentServer.
No protocol expansion needed for δ₂.

## δ₂ implementation plan

1. Rewrite `manox-napi/src/lib.rs` to use AgentServer + InProcessConnection
   - Replace `ActorHandle` with `AgentServer` + `InProcessConnection`
   - Replace `send(String)` with `send_to_server(FromClient::...)` (typed)
   - Replace `EventSink` callback with pump that serializes `FromServer` to JSON strings
2. Delete `crates/manox-actor/` crate
3. Delete `handle_command` + `ActorState` from `crates/manox-session-core/src/session.rs`
4. Delete `spawn_models_push` (replaced by client-side `ListModels` call)
5. Remove `manox-actor` from workspace Cargo.toml
6. Update `manox-napi` Cargo.toml to depend on `manox-session-core` + `manox-protocol`

## Prerequisites satisfied

- ✅ AgentServer handles all ClientNote variants (route_note)
- ✅ AgentServer handles all ClientCall variants (handle_call)
- ✅ Pump handles FromServer::Request (ServerCall) for adjudication
- ✅ Pump handles FromServer::Notification (ServerNote) for events
- ✅ InProcessConnection is Clone (workspace can send + pump can receive)
- ✅ ε serde round-trip tests validate protocol message integrity
