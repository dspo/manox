# AHP 映射表（P0）

> 本文件是 `~/.claude/plans/soft-marinating-diffie.md` 的 P0 交付物：把 v2 的每一份词汇
> 逐项对到 AHP 落点（action type / state 字段 / command / `x-` 扩展 / 丢弃），**无落点的一律
> 标红**，红行清单在文末。
>
> **状态**：进行中。§1（steer 持久化形态）已完成并附证据；§2 的逐项映射表尚未成文——
> 它是 P1 估算的前置。未完成部分不写"待补"以外的占位。

---

## §1 steer 持久化形态（P0 实证，已完成）

计划「Turn 边界与 part 身份 §1」要求先确认：**steer 在 journal 里到底长什么样**，再定后继
turn id 方案。结论如下，三条证据相互印证：

### 结论

1. **steer 不占独立 entry 种类，落盘形态就是一条普通的 user `message` 条目**，与开启 turn 的
   那条 user 消息同形。`SessionTreeEntry` 只有 `Message { .. }` 与 `Compaction { .. }` 两种
   条目族（`crates/manox-harness/src/core/session/mod.rs:26,40`），没有任何 steer 专属变体。
2. **「user 行已落盘」这一事实是 client 从 journal fold 出来的，不是 server 在线上推的**：
   `ThreadEvent::UserRowLanded` 在 `crates/manox-session-core/src/translate.rs:96-99` 被显式注释为
   *client-fold vocabulary: the SERVER never emits it* —— 也就是说该事件对应的持久事实只能是
   journal 里的 `message` 行，client 靠它退掉乐观回声。
3. **`ThreadEvent::SteerInjected { message_id }` 只是直播通知**，在
   `crates/manox-agent/src/thread.rs:1121`（run 收尾时对 `steered` 列表逐条 push）与
   `crates/manox-agent/src/thread.rs:290`（变体定义）可见；它不新增持久形态。

### 由此确定的 P1 约束

- **直播路径必须与 replay 一致**：既然 replay 时一条 user `message` 条目就是「turn 的起点」，
  那么 mid-turn 的 steer 在直播路径上也必须**关闭当前 turn、开启后继 turn**，否则同一段对话
  运行时是一种渲染、reload 之后是另一种（这正是 pi-ahp 踩过、计划要求照抄的判断）。
- **后继 turn id 采用计划建议的 `<rootTurnId>#<n>`**（`n` 为同一 root turn 下的后继序号）：
  与上面的事实相容，且不需要在条目里编码额外身份。
- `TurnFinished{stranded_steer_ids}`（`crates/manox-session-core/src/translate.rs` 同族）是
  「steer 未注入」的结案信号，映射时并入后继 turn 的收尾，不得丢弃。

### 证据缺口（如实记录）

`crates/manox-harness/tests/fixtures/session/*.txt` **没有 steer 场景**（`grep -rl steer` 零命中），
所以本结论建立在条目族穷举（证据 1）+ 事件语义注释（证据 2）+ 变体用法（证据 3）之上，
而非某一行 fixture。P1 期间若要更强的钉子，应新增一个 steer 的 journal fixture
（一条 user 消息插在 turn 中途），这正是计划里「双路径等价测试」要覆盖的形状。

---

## §2 逐项映射表

落点图例：**A** = AHP 原生（action / state 字段 / command），**X** = `x-manox` 扩展（计划
「`x-manox` 扩展面」已定点的那七项之一，或本表新提出的扩展），**🔴** = 丢弃。

### §2.1 `JournalWireEvent`（40）

| v2 条目 | AHP 落点 | 类别 |
|---|---|---|
| `Message{role:"user"}` | `chat/turnStarted`（`message` + `queuedMessageId`） | A |
| `Message{role:"assistant"}` | `chat/responsePart` / `chat/usage`（`Turn.usage`） | A |
| `Message{role:"tool"}` | `chat/toolCallComplete` 的 `result.content` | A |
| `UiNote` / `Custom` / `CustomMessage` | `chat/responsePart{kind:"systemNotification"}` | A |
| `TurnStart` / `TurnFinish` / `Stop` / `Error` | `chat/turnStarted` / `chat/turnComplete`（`TurnState::Complete\|Cancelled\|Error`）/ `chat/error` | A |
| `Retry` | `chat/responsePart{systemNotification}`（重试是通告，不是状态） | A |
| `AgentTextDelta` / `AgentThinkingDelta` | `chat/responsePart`（建 part）+ `chat/delta` / `chat/reasoning`（追加） | A |
| `ToolCall`（pending/running/done/error） | `chat/toolCallStart`·`chat/toolCallDelta`·`chat/toolCallReady`·`chat/toolCallComplete` | A |
| `ToolResult` | `chat/toolCallComplete`（`ToolCallResult`） | A |
| `ToolOutputChunk` | `chat/toolCallContentChanged` | A |
| `Approval{kind:"request\|decision"}` | `chat/toolCallReady{status:pending-confirmation, options[]}` + 客户端 `chat/toolCallConfirmed` | A |
| `Question{kind:"request\|decision"}` | `chat/inputRequested`·`chat/inputAnswerChanged`·`chat/inputCompleted`（`ChatInputRequest` 六种问法） | A |
| `ModelChange` / `ReasoningEffortChange` / `PermissionModeChange` | `session/configChanged`（`SessionConfigState.values`；无审批策略时用 `ToolCallConfirmationReason::Setting` 语义进 running） | A |
| `CwdChange` | `session/workingDirectorySet` | A |
| `ProjectChange` | `session/configChanged`（`project` 值）+ `SessionState.project` | A |
| `Title` | `session/titleChanged` | A |
| `PinnedArchived` | `session/isArchivedChanged`（原生位）+ `x-manox/pinnedChanged`（AHP 无 pin 位） | A + X |
| `Leaf` | `createChat{source}` 的 fork 语义；游标本身 → `x-manox/leafChanged` | A + X |
| `SubagentChild` | `chat/responsePart{toolCall}` 指向子 `ahp-chat:/`（`ToolResultSubagentContent`） | A |
| `SubagentProgress` | `x-manox/subagentsChanged` | X |
| `PlanModeChange` / `PlanModeRequest` / `PlanUpdate` / `PlanReview` | `x-manox/planModeChanged`·`x-manox/planUpdated`·`x-manox/planReviewChanged`（计划定点） | X |
| `Goal` | `x-manox/goalChanged` | X |
| `BrowserSuites` | `x-manox/browserSuitesChanged` | X |
| `BackgroundTask` | `x-manox/backgroundTaskChanged` | X |
| `ActiveToolsChange` | `x-manox/work activeToolsChanged`（内核诊断态，标准面无位） | X |
| `Compaction` / `CompactionStarted` | `x-manox/compactionComplete`·`compactionStarted`（**不得**错用 `chat/truncated`：那是客户端请求截断） | X |
| `BranchSummary` | `chat/responsePart{systemNotification}` | A |
| `Label` | `x-manox/labelChanged` | X |
| `SessionInfo` | `x-manox/sessionInfoChanged` | X |
| `Metrics` | `x-manox-metrics` channel 的聚合值（`x-manox/metricsChanged`） | X |

### §2.2 `HostEvent`（11）

| v2 | 落点 | 类别 |
|---|---|---|
| `Ready{epoch}` | `initialize` 结果（`protocolVersion` + `serverSeq`） | A |
| `Models(Vec<ModelInfo>)` | `root/agentsChanged`（`RootState.agents`） | A |
| `Commands(Value)` | `x-manox-commands://` channel + `x-manox/commandsChanged` | X |
| `ThreadsUpdated(Vec<ThreadListItem>)` | `listSessions` + `root/sessionSummaryChanged`（列表**不是** root state） | A |
| `SessionStatus{session_id, running, …}` | `SessionState.status` 位集（由 session/chat action 派生，无独立事件） | A |
| `SessionCreated` / `SessionDisposed` | `root/sessionAdded`·`root/sessionRemoved` + `session/chatAdded` / `disposeSession` | A |
| `WorkspaceUpdate` / `Projects` | `x-manox-workspaces://` channel（`manox-workspace` 域） | X |
| `TerminalsUpdated` | `root/terminalsChanged`（`RootState.terminals` 目录） | A |
| `Error` | JSON-RPC error 或 `chat/error` 响应部件 | A |

### §2.3 projection key（21）

`title`→`session.titleChanged`；`cwd`→`SessionState.workingDirectories`；`project`→`SessionState.project`；
`model`·`permission_mode`·`reasoning_effort`→`session/configChanged`；`plan_mode`·`plan`·`goal`·
`browser_suites`·`background_tasks`·`pinned`→ `x-manox`（见 §2.1 同名行）；`running`→`SessionState.status`
的 `InProgress` 位；`archived`→`IsArchived` 位；`pending_auth`→
`session/inputNeededSet{request:ToolConfirmation}`（**原生**，且 AHP 要求由它派生
`InputNeeded` 状态位）；`depth`·`branch`·`agent_label`·`self_author`→ `_meta`（`x-manox` 子对象）。

🔴 `has_interacted`：客户端从 `ChatState.turns` 是否非空即可推导（协议面已有该信息），不再占位。

### §2.4 请求 / 命令面

`ClientCall`(17)：`Initialize`→`initialize`；`OpenSession`→`subscribe`；`ListThreads`→`listSessions`；
`ListModels`→root `agents`；`ListCommands`→`x-manox-commands://`；`TerminalAttach`/`TerminalSnapshot`→
`subscribe ahp-terminal:/`；`RegisterSessionTools`→`SessionInputRequestKind::ToolClientExecution`（原生）；
`CreateSession`→`createSession`；`Submit`→`dispatchAction{chat/turnStarted}`；`Steer`→
`chat/pendingMessageSet{kind:"steering"}`；`PageHistory`→`fetchTurns`；`Workspace{call}`→`x-manox-workspaces://`；
`GetConversationInfo`→`x-manox-metrics`；`CancelDelivery`→`x-manox/cancelDelivery`（server→client 请求的撤回）；
`ForkSession`→`createChat{source:{kind:"fork"|"sideChat"}}`。

🔴 `ClientCall::ModelChat` + `ClientNote::CancelModelChat` + `ServerNote::{ModelText, ModelThinking, ModelToolCall, ModelChatDone}`：
唯一消费者（VS Code LM provider）已不存在，`model_chat.rs` 按计划删除。

`ClientNote`(27)：`DetachSession`→`unsubscribe`；`DisposeSession`→`disposeSession`；
`DropQueued`→`chat/pendingMessageRemoved{kind:"queued"}`；`CancelTurn`→`chat/turnCancelled`；
`SetModel`/`SetReasoningEffort`/`SetApprovalMode`→`session/configChanged`；`SetCwd`→`session/workingDirectory*`；
`SetPlanMode`/`PlanSeedExecution`→`x-manox/planModeChanged`·`x-manox/planExecute`；`SetBrowserSuite`→
`x-manox/browserSuitesChanged`；`Compact`→`x-manox/compactionStarted`；`Goal`→`x-manox/goalChanged`；
`StopBackgroundTask`→`x-manox/backgroundTaskChanged`；`ArchiveThread`→`session/isArchivedChanged`；
`PinThread`→`x-manox/pinnedChanged`；`InsertThreadBefore`/`InsertGroupBefore`→`x-manox/orderChanged`；
`TerminalInput`/`TerminalResize`→`terminal/input`·`terminal/resized`；`AppendUserMessage`→
`chat/pendingMessageSet{kind:"queued"}`；`AppendUiNote`→`chat/responsePart{systemNotification}`；
`Shutdown`→`x-manox/shutdown`（连接级）。

🔴 compat 三件 `ClientNote::{CreateSession, Submit, Steer}`：v2 遗留双发腿，随 v2 一并消失。

`ServerCall`(6)：`Approve`→`chat/toolCallReady` + `chat/toolCallConfirmed`（原生审批面）；
`AskUserQuestion`→`chat/inputRequested`（原生 elicitation）；`BrowserOp`·`ClipboardRead`·`OpenExternal`→
`x-manox/*` server→client 方法（计划定点，需要请求-应答）；`InvokeClientTool`→
`SessionInputRequestKind::ToolClientExecution` + `x-manox/invokeTool` 兜底。

`ServerNote`(12)：`Ready`→握手；`SessionCreated`/`SessionDisposed`/`Error`→ 见 §2.2；
`ThreadsUpdated`/`Models`/`Commands`→ 见 §2.2；`DeliveryCancelled`→ 裁决撤回经 `x-manox/cancelDelivery`
与 `session/inputNeededRemoved` 表达。

### §2.5 `ThreadEvent`（37，直播面）

| 落点 | 事件 |
|---|---|
| 已在 §2.1 有对应（经 `translate` 同一条路径） | `AgentText`·`AgentThinking`·`ToolCall`·`ToolResult`·`ToolOutput`·`SubagentStarted`·`SubagentProgress`·`SubagentChild`·`ToolCallAuthorization`·`TurnStarted`·`Stop`·`TurnFinished`·`Retry`·`Error`·`PermissionModeChanged`·`ModelChanged`·`ReasoningEffortChanged`·`GoalChanged`·`CwdChanged`·`CompactionStarted`·`Compaction`·`PlanReady`·`PlanUpdated`·`PlanModeChanged`·`BrowserSuitesChanged`·`BackgroundTaskUpdated`·`SteerInjected`（见 §1 的 turn 边界规则） |
| `_meta` | `TitleChanged` |
| 🔴 | `UserRowLanded`（client 从 journal fold，非 server 事件）·`PeerMessage`（多 agent 协作不在本期）·`HistoryProgress`·`HistoryRestored`（UI 瞬态）·`PrefixStability`·`CacheInvalidation`·`SideCallMetricsUpdated`·`MainCallMetricsUpdated`·`TokenUsageUpdated`（诊断指标，聚合进 `x-manox-metrics`） |

## §3 统计与红行清单

| 词汇 | 总数 | AHP 原生 | `x-manox` | 🔴 丢弃 |
|---|---|---|---|---|
| `JournalWireEvent` | 40 | 20 | 19 | 1（`Metrics` 并入 `x-manox-metrics` 聚合，不计原生） |
| `HostEvent` | 11 | 8 | 3 | 0 |
| projection key | 21 | 9 | 11 | 1（`has_interacted`） |
| `ClientCall` | 17 | 11 | 4 | 2（`ModelChat`；`Workspace` 计入 `x-manox`） |
| `ClientNote` | 27 | 10 | 14 | 3（compat 三件） |
| `ServerCall` | 6 | 3 | 3 | 0 |
| `ServerNote` | 12 | 5 | 3 | 4（ModelChat 侧流） |
| `ThreadEvent` | 37 | 28 | 1 | 8 |

**红行清单（需评审后才能进 P1 估算）**

1. `JournalWireEvent::Metrics` — 无 AHP 位；聚合进 `x-manox-metrics`，不逐条上 wire。
2. `projection::has_interacted` — 客户端可从 `ChatState.turns` 推导。
3. `ClientCall::ModelChat` + `ClientNote::CancelModelChat` + 4 条 `ServerNote::Model*` — `model_chat.rs` 删除后无消费者。
4. `ClientNote::{CreateSession, Submit, Steer}` — v2 双发遗留腿，随 v2 消失。
5. `ThreadEvent::{UserRowLanded, PeerMessage, HistoryProgress, HistoryRestored, PrefixStability, CacheInvalidation, SideCallMetricsUpdated, MainCallMetricsUpdated, TokenUsageUpdated}` — 各自理由见 §2.5。

> 计数口径：`JournalWireEvent` 的 X 列含 12 项计划已定点的七类扩展；`ThreadEvent` 的 A 列是
> 「经同一 translate 路径到 §2.1 落点」的合计，不含 `_meta` 与红行。
