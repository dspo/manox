# manox Architecture v2 —— 完备设计与迭代计划

> 主文档，唯一事实源。取代 `~/.manox/plans/dsh-event-journal-adoption-plan.md`（其 J1–J11 决策已全部并入本文；M0–M7 阶段被 §K 迭代计划取代）。
> 参考架构：deepseek-harness（dsh）。参考生态结论：主仓 63 插件 + 社区 725+ 条目全部只消费五种底座原语、零内核修改。
> 执行契约：**本地迭代、本地提交与合并、终局单 PR**；任务正交并行，单任务 256K–768K tokens；委派 subagent（packet 装配规则见 §K.4），主线由主 agent 验收集成；部分任务主 agent 亲做。

---

## A. 设计公理与不变量（L1–L12）

- **L1 句柄锁不可重入**（沿旧）：任何 `Handle::read/with_mut` 闭包内不得再调同一 handle 的方法。
- **L2 前端能力一律 `BoxFuture`**（沿旧）：`AsyncApp` 非 `Send`；能力超时由 AgentServer 侧 `RpcPeer` 套。
- **L3 一切状态变更皆日志条目**：Thread 内核与 AgentServer 的可观测状态只能经 journal 条目变更。「字段变了但没有携带事件」在结构上不可能。
- **L4 seq 单点盖章**：seq 只在日志 append 点赋值（`with_mut` 出口）。seq = 活动链上条目的深度（0-based、稠密）。
- **L5 快照不丢、溢出即重同步**：`Snapshot/Projections/StreamEnd` 永不受背压 Drop；Entry 流队列满 → `StreamEnd{Resync}` → 客户端重新 follow。禁止 per-client 服务端重放缓冲。
- **L6 客户端零领域 fold**：UI 值要么来自日志条目（records/delta），要么来自投影（key→{value, asOfSeq}，higher-seq-wins）。客户端禁止解析领域身份（模型引用只显示）。
- **L7 响应不带领域数据**：ClientCall 写操作只回 receipt；领域结果经日志条目/投影到达。
- **L8 wire 身份 canonical**：模型在 wire 上永远是 `{provider_registration}/{model_id}` 限定串；裸 id 仅服务端输入兼容，解析收敛在 `resolve_model_ref` 一处。
- **L9 上层只经底座表达状态**：任何 UI 值必须且只需五者之一承载——J 日志条目 / P 推送投影 / Q 按需 fold / H host 事件 / client-owned 本地态。某需求无法表达 = 底座缺口，先扩底座，严禁组件层私开同步通道。
- **L10 重放等于内存**：`Thread` 全部可观测状态可由 journal 重放确定性重建（回放一致性测试是内核合并门禁）。facade「镜像内核状态」的双份数据在迁移完成后消灭。
- **L11 单网关**：一个进程一个 `AgentServer` 实例；所有前端（GPUI/webui/VS Code）经 `RpcConnection`（in-proc 或 loopback WS+token）连它；传输无关由双路径一致性测试保证。
- **L12 声明面即公开契约**：journal 条目词汇、投影 key 表、host 事件表、协议帧四张声明面（§J）稳定公开、版本化（journal header version、Initialize 携带 protocol epoch）；unknown 变体容忍（丢帧记日志不断连）。生态工具可直接读日志文件（dsh-replay/dsh-timesheet 先例）。

## B. 分层拓扑与进程模型

```
L6 域旁路   terminal(PTY 自有通道) · ModelChat 侧流 · MCP · LSP        ——不进会话日志
L5 扩展面   webui slot registry · 插件 bundle 静态通道 · plugin routes · 宿主服务缝清单
L4 UI       GPUI agent-ui（selector-only） · webui React（slots+hooks） · VS Code webview（复用 webui）
L3 客户端SDK JournalStream 引擎(Rust+TS 双胞胎) · SessionStore · selector · echo/retire · 重连
L2 协议     manox-protocol v2：帧/Call/Note/ServerCall/HostEvent · ts-rs · TS 守卫 · fixtures
L1 网关     AgentServer 单例：FollowStream · PageHistory · ProjectionRegistry · RPC回执
            · HostEvent 总线 · ServerCall waterfall · 能力路由 · plugin route 注册
L0 内核     ThreadCore + Journal v4（append-only、链稠密 seq）· engine · 持久化 · provider/credentials
```

进程模型：`manox` bin 创建唯一 `AgentServer`；GPUI 前端经 `in_process_pair`；`manox-webui`（axum）持同一实例暴露 `/ws`（loopback+token）供浏览器与 VS Code（napi 经 WS 或 in-proc，二者同一实例）。桌面与 webui 各起实例的现状（workspace.rs:765 vs pump.rs:14-18）在 T5 消灭。

## C. Journal v4 完备规格

### C.1 文件与信封
- 路径 `~/.manox/sessions/<thread_id>.jsonl`。第 0 行 header：`{"type":"session","version":4,"id","timestamp","cwd","parentSession"?,"metadata"?}`。
- 条目行：`{"seq":u64,"id":uuid,"parentId":uuid,"timestamp":iso,"type":camelCase,...payload}`。**信封键独占规则**：`seq/id/parentId/timestamp/type` 为信封保留键，事件载荷不得使用同名键（`#[serde(flatten)]` 下同名会互抢/产生重复键）——tool 事件的句柄叫 `callId`，subagent 事件的句柄叫 `agentId`。
- **seq = 活动链深度**：链上稠密 0-based。分叉共享前缀 seq、新后缀续编号；中插（merged follow-up）= 新链 + `leaf` 重定向（现状语义 + seq）。加载时沿 leaf 链校验稠密，违例报错。
- **v3 兼容**：旧文件（version:3、无 seq）读入时按链深回填 seq，内存使用；下次 append 时以 v4 写出（懒迁移）。`leaf.targetId` 游标重定向语义不变。

### C.2 条目词汇表（kernel `JournalEntry` enum，serde tag="type" camelCase；即 wire `JournalWireEvent`）

| 组 | 条目 | 载荷要点 |
|---|---|---|
| transcript | `message` | user/assistant/tool 消息；assistant 携带 `usage`（input/output/cacheRead/cacheWrite/reasoning）；条目携带 `origin?`（乐观回显退休；**as-built**：Submit 的 origin_rpc 经 `SessionCmd::Prompt` → `Session::set_pending_user_origin` → 持久化中间件在本 turn 首个 user 消息落盘时一次性消费，`append_message_with_origin` 钉入条目） |
| transcript | `ui_note` | 现 AppendUiNote 改 durable |
| lifecycle | `turn_start` / `turn_finish{cancelled,failed,strandedSteerIds}` / `stop{reason}` / `retry{attempt,maxAttempts,delaySecs,reason}` / `error{message}` | `anyhow::Error` 过线/落盘转 `{message}` |
| 流式 delta | `agent_text_delta{delta}` / `agent_thinking_delta{delta}` / `tool_call{callId,name,title,status,input}` / `tool_result{callId,output,isError}` / `tool_output_chunk{callId,chunk}` / `subagent_child{agentId,event}` / `subagent_progress{agentId,...}`（≥500ms 或状态变化才记） | dsh chunk 全落盘同款；分页读取端可做 chunk-run 打包（优化，不改语义）；`callId`/`agentId` 遵守 §C.1 信封键独占规则（**as-built**：kernel 侧字段名为 `delta`，wire 映射在 translate 层改名 `s`→`delta` 或直接沿用，见 T4 报告） |
| 状态变更 | `model_change{from?,to}`（to=canonical）/ `cwd_change{cwd}`（**as-built**：沿用 v3 字段名 `cwd`）/ `project_change{path?}` / `permission_mode_change{mode}` / `reasoning_effort_change{effort}`（**as-built**：复用既有 `thinking_level_change` 条目，字段 `thinking_level`）/ `plan_mode_change{enabled}` / `plan_update{snapshot}` / `goal{goal?}` / `title{title}` / `browser_suites{suites}` / `background_task{snapshot}` / `approval{kind:request|decision, authId, payload}` / `pinned_archived{pinned,archived}` | approval request+decision 双态入日志，投影 `pending_auth` 的 fold 源 |
| 压缩/树 | `compaction{...}` / `compaction_started{tokensBefore}` / `branch_summary` / `label` / `session_info` / `active_tools_change` / `custom` / `custom_message` / `leaf{targetId}` | 沿用现 SessionTreeEntry 语义 |
| metrics | `metrics{metricType,data}` | 诊断面；入日志但可声明「低优先」 |

**供 T6/T7 的 as-built 摘要**：宿主侧 journal 供给 = `ThreadHandle::subscribe_journal_feed()`（`JournalFeed::{Event{seq,entry}, Lagged}`）+ `ThreadHandle::journal_snapshot()`（整链冷读）；投影 = `crates/manox-session-core/src/projections.rs::ProjectionSet`（seed+apply+drain_changed/baseline）；waterfall = `src/waterfall.rs::Waterfall`（reply/expire/cancelled_recipients）；origin = message 条目 `origin` 字段（wire 名 `origin`）。

**不成为条目的**（快照边界语义承载）：`HistoryProgress/HistoryRestored` → Snapshot 帧边界；`compaction_started` → 并入 `compaction` 的前置状态或独立条目（选独立条目，UI spinner 需要）。

### C.3 内核改造
- `ThreadCore { state: Thread, journal: SessionLog, subscribers: Vec<Sender<Arc<JournalEvent>>> }`；`JournalEvent{seq, entry}`。
- `with_mut` 出口三段（现 thread.rs:490-503）：锁内收集 `pending_events` → **单点赋 seq 并 append 日志（内存+文件）** → 广播 `JournalEvent`。泵是唯一订阅者，通道满即 await（消灭现 try_send 满丢，thread.rs:505-522）。
- `ThreadEvent`（30 变体）保留为内核内部事件面；新增 `ThreadEvent → JournalEntry` 的序列化映射与 `JournalEntry → ThreadEvent` 反投影（桌面视图复用）。新 durable 事件（ui_note/approval/project_change/pinned_archived/title...）直接产生条目。
- 读 API：`journal.cursor() -> u64`、`journal.slice(from..to) -> Vec<JournalEvent>`、`journal.replay() -> Thread`（L10 门禁）。compaction 后 `slice` 的 records 视图从 `firstKeptEntryId` 起（seq 连续性不变）。
- 写放大对策：组提交（批量 flush，默认不逐条 fsync）；页读 chunk-run 打包。唯一允许的回退是 `subagent_progress` 降频，不得回退「条目皆可重放」。

## D. 协议 v2 完备规格（manox-protocol）

### D.1 帧（信封）
```rust
enum FromClient {
  Request { id: MsgId, call: ClientCall },
  Notification { note: ClientNote },
  Reply { id: MsgId, outcome: RpcOutcome },
  StreamOpen { stream_id: StreamId, kind: StreamKind },
  StreamCancel { stream_id: StreamId },
}
enum FromServer {
  Response { id: MsgId, outcome: RpcOutcome },
  Request { id: MsgId, call: ServerCall },          // waterfall / capability
  Notification { host: HostEvent },
  StreamItem { stream_id: StreamId, frame: StreamFrame },
  StreamEnd { stream_id: StreamId, reason: StreamEndReason },
}
enum StreamKind { FollowSession { session_id: String, max_messages: Option<u32> } }
enum StreamFrame {
  Snapshot(SessionSnapshot),
  Entry { seq: u64, event: JournalWireEvent },
  Projections(ProjectionsFrame),
}
struct SessionSnapshot { session_id, header: ThreadHeader, cursor: u64,
  records: Vec<JournalWireEvent>, has_more: bool,
  projections: BTreeMap<String, JsonValue>, projections_as_of_seq: u64 }
struct ProjectionsFrame { session_id: String, as_of_seq: u64, values: BTreeMap<String, JsonValue> } // 只含变更 key
enum StreamEndReason { Closed, Cancelled, Resync, Failure { code: String, message: String } }
```

### D.2 ClientCall（Request，L7：写只回 receipt）
`Initialize{client_id, capabilities: Vec<HookKind>, sessions: Vec<String>, protocol_epoch: u32}`；`CreateSession{cwd, project?, initial_model?, approval_mode?, reasoning_effort?}`→`{session_id}`（服务端走 `new_in_project` 等价路径；对已存在 session 幂等返回既有 id）；`OpenSession{session_id}`→receipt（历史经 follow 流到达）；`Submit{session_id, text, images?, origin_rpc}`→`{accepted, message_id?}`；`Steer{session_id, message_id, text?, origin_rpc}`→receipt；`PageHistory{session_id, through_seq: i64(-1=最新), before_seq?, max_messages?}`→`{records, has_more, cursor}`（冷读不激活 engine，jsonl 直读）；`GetConversationInfo{session_id}`→fold 载荷（§E.3；按 `(thread_id,cursor)` 缓存）；`ListThreads`/`ListModels`/`ListCommands`→快照（纯读，允许领域数据——L7 只约束写）；`TerminalAttach/TerminalSnapshot/ModelChat*`（沿旧）。

### D.3 ClientNote（fire-and-forget 命令）
`DetachSession, DisposeSession, DropQueued, CancelTurn, SetModel{session_id, model: ModelRef(canonical)}, SetReasoningEffort, SetApprovalMode, SetCwd, SetPlanMode, PlanSeedExecution, Compact, Goal, StopBackgroundTask, ArchiveThread, PinThread, FocusThread, TerminalInput, TerminalResize, CancelModelChat, Shutdown`。

### D.4 ServerCall（waterfall，§K 见 J7 语义）
`Approve{delivery_id, auth_id, tool_name, summary, input}`；`AskUserQuestion{delivery_id, ...}`；`PlanVerdict{delivery_id, plan_file, title, body}`——三者 fan-out 给 `owner(session) ∩ capability` 全部连接，全部 `next` 才放行、任一 `rejected` 即取消并广播 cancel；单 owner 行为与现状一致。`BrowserOp/ClipboardRead/OpenExternal` 定向单连接（owner∩capability 首个，超时作废按 rejected）。

### D.5 HostEvent（全局通知，取代 server 端领域 note）
`Ready{epoch}`；`Models(Vec<ModelInfo>)`（provider reload 即推）；`Commands(...)`；`ThreadsUpdated(Vec<ThreadSummary>)`（元数据变更时全量快照：title/pin/archive/model/分组）；`SessionStatus{session_id, running?, errored?, unread?, pending_auth?, pending_plan?, background_work?}`（小 delta 高频；turn 生命周期/审批挂起/后台任务时广播给**全部**连接；客户端单调镜像：unread 只增至 focus 清零、errored 边沿置位、running 最新为准）；`SessionCreated{session_id, header}`/`SessionDisposed{session_id}`（owner 集合内控制）；`Error{message}`。

### D.6 死亡清单（迁移完成后删除）
`ServerNote::{AgentText, AgentThinking, ToolCall, ToolResult, ToolOutput, TurnStarted, TurnFinished, Stop, Retry, Compaction*, Subagent*, ModelText/Thinking/ToolCall/ModelChatDone, ThreadInfo, ThreadHistory, ThreadsUpdated, Models, Commands, Usage, UsageSnapshot, TokenUsage, CurrentModel, PlanReady?, PlanUpdated, PlanModeChanged, GoalChanged, CwdChanged, PermissionModeChanged, ReasoningEffortChanged, BrowserSuitesChanged, BackgroundTaskUpdated, SteerPending/Injected, ApprovalDecision, Branch, GitStats, HistoryProgress, PeerMessage?, CacheInvalidation, Error}`——分别由 Entry 条目 / 投影 / HostEvent / Snapshot 边界取代。`translate.rs` 的镜像臂全灭，ServerCall 生成臂保留迁入新泵。

### D.7 背压与错误
- 策略表（取代 transport.rs:36-49）：`StreamItem(Snapshot|Projections)` 与 `StreamEnd` → 永不 Drop；`Entry` → 有界（4096）满即发 `StreamEnd{Resync}`；控制帧（Request/Response/Notification）→ 阻塞不丢。
- `RpcError{code, message}`，code 集：`session/not-found, session/busy, gateway/bad-request, gateway/internal, resync-required, model/unresolvable`。

### D.8 TS 侧
ts-rs 绑定再生成；帧层手写 exact-key 守卫（dsh stream-protocol.ts:270-291 同款）；cargo 测试导出真实帧 JSON fixture（`crates/manox-protocol/fixtures/`）→ vitest 断言守卫解析（双路径一致性的 TS 侧，M0 围栏）。

## E. 投影注册表（P 面）

### E.1 契约
```rust
trait Projection {
  const KEY: &'static str;
  type Value: Serialize + DeserializeOwned + PartialEq;
  fn seed(t: &Thread) -> Self::Value;                    // 快照初值（冷启动一次）
  fn fold(v: &mut Self::Value, e: &JournalEntry);        // 增量
}
static PROJECTIONS: &[&dyn ProjectionDef] = &[ ... ];    // 声明面（L12）
```
AgentServer 每会话持投影实例组；泵转发条目时 fold；变更 key 随 `Projections` 帧发布（as_of_seq=触发条目 seq）。快照带全量。客户端 per-key `{value,seq}`，higher-seq-wins。

### E.2 key 全表（首版 20 个）
`title, cwd, project, model{provider,id}, permission_mode, reasoning_effort, plan_mode, plan, goal, running, has_interacted, pinned, archived, depth, branch, browser_suites, pending_auth, background_tasks, agent_label, self_author`。
（`running` fold `turn_start/turn_finish/stop/error`；`has_interacted` fold user `message`；`pending_auth` fold `approval` 双态。）

### E.3 Q 面（按需 fold，不占推送）
`GetConversationInfo` 返回：`{title, cwd, project, model, context_window, turns, messages, models:[{provider,model,input,output,cacheRead,cacheWrite,reasoning,calls,lastTotal,contextWindow,hitRate,pct}], cumulative_cost, git:{branch, ahead, behind, dirty}}`——服务端折叠 journal（conversation-info/foldUsage 直译 + tokenMeter 语义 + git stats 并入），按 `(thread_id, cursor)` 缓存，cursor 前进才重算。客户端边沿信号=自己 records 窗口的「已提交消息数」，变化才调（去抖 120ms + visibility 感知）。

## F. 客户端 SDK（L3 层）

### F.1 JournalStream 引擎（Rust `crates/manox-protocol/src/journal.rs` + TS `webui .../state/journal.ts`，规则逐条等价）
泛型 `JournalStream<P,E>`（cursor=u64），注入代数：`entries(page)/hasMore/first/last/compare/follows/publish/failed`。规则（dsh journal-stream.ts:296-373 直译）：
1. 打开：首帧必为 `Snapshot`，校验页内条目互相邻接、页尾=cursor；发布 `Replace{records, projections}`。
2. Entry：`last<=已见` 丢弃（幂等）；部分重叠=协议违规（报 failed）；`follows` 不成立=缺口 → `PageHistory(through=缺口尾)` 补齐 + 期间到达条目按 seq 归并 + 整体 `Replace` 发布；补页尾仍不达=违规。
3. 重开（连接换代）：`restart()` → 重新 follow → 新 Snapshot cursor 必须 ≥ lastCursor，否则违规；保留旧窗口直到新快照落地（无感重连）。
4. `prepend(page)`：历史翻页，不连续=违规。
5. 属性测试：随机 drop/重排/断流/重连序列 → 收敛等于服务端状态。

### F.2 SessionStore（Rust `client_store.rs` v2 / TS `store.ts` v2）
`{ window: Vec<JournalRecord>, projections: Map<key,{value,seq}>, echo: Map<rpcId, EchoEntry>, status: ConnectionStatus }`；apply `Change::{Replace,Prepend,Append}`；display/气泡/工具卡 = 对 window 的**通用 UI fold**（非领域状态）。selector 读面（L9/J11）：Rust `store.with(|s| R)`；TS `useStore(selector)`（useSyncExternalStore）。echo 由条目 `originRpc` 退休。选中/焦点/草稿 = client-owned，永不读镜像（T2③ 根治）。

## G. UI 组合层（webui 优先）

- TS slot registry（dsh ui-slots 直译）：`SlotMap` 声明合并（module augmentation）、kind=`single|list|keyed|chain`、scope=`root|session`；标准 hooks `useSessions/useSession(selector)/useProjection(key)/useThreadStatus()`。纪律：面板只 `slots.register/inject`，永不 import 其他面板；single 可 shadow、list 有序共存。
- 首批 slot 落位：`sidebar.workspaces`、`sidebar.footer.action`、`conversation.session.header.utilities`、`conversation.composer.dock`、`conversation.chat.node`(keyed)、`settings.section`、`shell.overlay`。
- 桌面 GPUI：selector-only（编译期组合），不做动态 slot——记入远期。

## H. 扩展面（宿主服务缝 + 插件通道）

- 宿主服务缝清单（现状+新增）：`provider_registry`/`credentials`/`tools`(harness)/`thread_store`/**`journal_query`(新: slice/page/replay)**/**`projections`**(新)/**`host_events`**(新)/**`plugin_routes`**(新: axum 注册 `/api/plugin/<name>/*`，同源+token)/`settings`/`capabilities`(前端)。缝间互不 import 实现。
- webui 插件 bundle（静态先行）：`apps/web/webui/plugins/<name>/{host.ts?, client.ts, manifest.json}`，构建期内联 dist；host 面 = plugin route + journal fold（conversation-info 模式）；client 面 = slot 注册。动态加载（dsh `__ModuleLoader__` 式）为远期第二档。
- 「对话信息」卡的最终形态 = 第一个插件（selector + GetConversationInfo，零 note 零 emit），作为组合层的验收样本。

## I. 安全与信任
loopback+token 沿用；credentials 永不下发浏览器（keychain/env/literal/shell 四源解析在 host 侧）；能力 fail-closed 沿用；插件即信任决策（安装时确认，面板代码进 bundle 即可信）；waterfall 死等由 RpcPeer 超时作废按 rejected。

## J. 测试与门禁体系（每任务验收 = 门禁绿 + 专项）
1. 全仓门禁：`cargo fmt`、`cargo clippy --workspace --all-targets -- D warnings`、`cargo test --workspace`、webui `npm run typecheck && npm run test`。
2. 回放一致性（L10）：落盘重载 == 内存（display/投影/游标）。
3. JournalStream 属性测试（F.1.5）。
4. 声明面覆盖（L12）：journal 条目/投影 key/host 事件/协议帧四张表，emit 点 100%（coverage 测试：脚本化会话驱动后断言每个声明面出现在 FromServer 流；扩展 `dual_path_transport_consistency`）。
5. 双路径一致性：in-proc ≡ serde ≡ TS 守卫（fixtures）。
6. grep 门禁（终局）：视图/组件层不得 import 协议发送面；`ServerNote::ThreadInfo|UsageSnapshot` 等死亡清单零残留。
7. 病灶回归：has_interacted 首交互即显（投影）；消费统计实时+历史（Q 面折叠）；项目/模型继承（CreateSession 意图）；选中一次生效（client-owned）；模型不串号（canonical+零解析）。

---

## K. 迭代计划（本地单 PR）

### K.1 git 策略
- 集成分支 `arch/dsh-v2`（自 main 建；已含待提交的 session-core 修复 commit 与本文档 commit）。
- 任务在**本地 worktree** 分支 `task/<Tn>-<slug>`（自 `arch/dsh-v2` 切出）实施；**绝不 push、绝不建 PR**；交付=本地分支上的提交序列+报告。
- 主 agent 验收（§K.4）后 `git merge --no-ff` 回 `arch/dsh-v2`（同仓合并，本地进行）。
- 终局（T10）：push `arch/dsh-v2` → 对 main 开**一个大 PR**；CI 绿后按用户指示合并。

### K.2 任务表

| 任务 | 波次 | 内容 | 委派 | 依赖 | 预算 |
|---|---|---|---|---|---|
| T1 journal 内核 | 1 | C 全节 | **主 agent 亲做** | — | ~512K |
| T2 协议 v2 crate | 1 | D 全节 + J.4/J.5 声明面与 fixtures | general-purpose | 文档 | ~256–384K |
| T3 JournalStream 双引擎 | 1 | F.1（Rust+TS）+属性测试 | general-purpose | 文档 | ~384K |
| T4 服务端流服务 | 2 | FollowStream/PageHistory/GetConversationInfo/泵改接 journal 广播 | general-purpose | T1,T2 | ~384–512K |
| T5 服务端投影+host事件+waterfall+单例 | 2 | E、D.4/D.5、B 进程模型 | **主 agent 亲做** | T1,T2（与 T4 串行，同文件域） | ~512–768K |
| T6 桌面客户端迁移 | 3 | client_store v2/selector/multiplexer/删 server_note_translate/CreateSession 意图发送 | general-purpose | T3,T4,T5 | ~512–768K |
| T7 webui 客户端迁移 | 3 | store v2/bridge 回执路由/echo 退休/canonical 显示/SessionStatus 镜像/选中态 client-owned/删 GetUsage 死路 | general-purpose | T3,T4,T5 | ~512–768K |
| T8 webui 组合层+插件复刻 | 4 | G、H、对话信息卡插件化 | general-purpose | T7 | ~512–768K |
| T9 VS Code 同步 | 4 | napi/vscode-bridge 切 v2 帧与守卫 | general-purpose | T7 | ~256K |
| T10 拆旧+终局门禁+单 PR 组装 | 5 | D.6 死亡清单、J.6 grep、文档、CI | **主 agent 亲做** | 全部 | ~256–512K |

波次并行：波1={T1,T2,T3}（互不触碰对方文件域）；波2={T4→T5 串行}；波3={T6,T7 并行}；波4={T8,T9 并行}；波5={T10}。

### K.3 任务规格（packet 正文模板）

**T2 协议 v2 crate（packet 现文）**
- 范围：`crates/manox-protocol`——新增 §D.1 全部帧类型、`JournalWireEvent`（§C.2 词汇表逐变体）、`ModelRef`、`StreamId`；重构 `ClientCall/ClientNote/ServerCall/HostEvent` 至 §D.2–D.5；死亡清单类型**暂保留**（加 `#[deprecated]` doc 标记，T10 删）；背压策略表改 §D.7；`SURFACE` 声明模块（四张表：`JOURNAL_ENTRIES/PROJECTION_KEYS/HOST_EVENTS/FRAMES`，const 数组）；emit 覆盖测试 harness（脚本化 `FromClient` 序列驱动一个 fake server 断言声明面出现）；ts-rs 绑定再生成 + TS exact-key 守卫（`bindings/guards.ts`）+ fixtures 导出测试（真实帧 JSON 落 `fixtures/`）。
- 现状锚点：`msg.rs`（信封 58-90）、`client.rs`（32-187）、`server.rs`（24-388）、`transport.rs`（策略表 36-49、`in_process_pair`:139）、既有测试 `dual_path_transport_consistency`（agent_server.rs:3056）。
- 门禁：J.1 全绿 + 覆盖 harness 绿 + `npm run typecheck`（若 webui 引用旧类型则 stub re-export 保持编译）+ 文档注释完整（每个 public 类型一句话语义 + 所属声明面）。

**T3 JournalStream 双引擎（packet 现文）**
- 范围：Rust `crates/manox-protocol/src/journal.rs`（泛型引擎 + 规则 F.1 全条 + `proptest` 属性测试：模型=随机事件序列+随机丢/重排/重连，断言 publish 流收敛等价理想 fold）；TS `apps/web/webui/src/sidebar/webview/state/journal.ts`（同规则）+ vitest（含消费 T2 fixtures 的帧级用例）。引擎不 import 会话领域类型（纯代数）。
- 门禁：J.1（Rust 侧）+ webui vitest 绿 + 双实现共用同一组 JSON 测试向量（`test-vectors/journal-cases.json` 双端加载，等价性硬保证）。

**T1 journal 内核（主 agent 亲做，规格）**
- 范围：`crates/manox-harness/src/core/session/{jsonl.rs,mod.rs}` v4（seq/懒迁移/稠密校验/新条目类型）；`crates/manox-agent/src/thread.rs`（ThreadCore 出口单点盖章、`JournalEvent` 广播 await 化、`ThreadEvent↔JournalEntry` 映射、新 durable 事件、`journal.slice/cursor/replay` API）；engine 对接（usage 载荷入 assistant message 条目）；L10 回放一致性测试（J.2）。锚点：jsonl.rs:27/186/249、thread.rs:388/490-522、engine.rs:3789(sync_usage)/1574 等 settle 点。

**T4/T5/T6/T7/T8/T9/T10**：按同模板在 dispatch 时从本文对应章节装配（范围=节号列表；锚点=本仓库现状 file:line；门禁=J 对应项 + 任务专项）。T5 关键点：`follow` 泵订阅改为 journal 广播、Snapshot 组装=records+投影 baseline、dispose 只影响请求方、`create_session` 幂等、webui/vscode 复用单实例、WS client_id 持久化钩子（web-bridge sessionStorage）。

### K.4 委派与验收协议
1. packet 装配：prompt 必含（a）本文对应章节**全文引用**；（b）仓库现状锚点 file:line；（c）接口契约（依赖任务的 public API 签名）；（d）验收命令清单；（e）git 纪律（worktree、`task/Tn` 分支、禁 push/PR）；（f）「未覆盖情况停下报告，不做任务外设计决策」。
2. 验收（主 agent）：diff 全读 + 门禁复跑 + 专项抽测 + 对抗性检查（故意构造缺口/重连/背压场景跑引擎）；通过则本地 merge；不通过则一次性给出修正清单退回。
3. 集成顺序即 K.2 波次；每波结束在集成分支打 tag `wave/N`。

### K.5 风险与回滚
- 每任务分支独立，验收不过不合并——集成分支任何时刻可回退到上一个 `wave/N`。
- jsonl v3 懒迁移保底：迁移期读取兼容 v3/v4 双格式；`threads.db` 不做 schema 破坏（`thread_events` goal 索引原样）。
- 迁移窗口双协议（新帧与旧 note 并存）从 T4 起至 T10 拆旧止，终局 grep 门禁保证无双协议残留。
- 大 PR 审查负担：以 wave tag 分 commit 段 + 本文档作为 PR 描述骨架。

### K.5.1 T10 拆除与集成清单（**已完成**，2026-09-05 终局门禁全绿）

**终局状态**：translate 只余裁决路由；桌面渲染与 restore 全走 v2 流（含重开回归锁 `reopen_snapshot_restores_transcript_and_rearms_rebuild`）；v1 快照发射面（ThreadHistory/ThreadInfo/GetUsage/GetCurrentModel/残余 PermissionModeChanged/SteerPending）删除；37 个 DOOMED ServerNote 变体删除（保留 11 个：owner 控制 ×3、过渡列表通道 ×3、服务端 Error、ModelChat 侧流 ×4）；双端 v1 fold 清除；绑定/守卫/fixtures 再生成；桌面 usage 面板接 Q 面（committed 边沿，含回归测试）；全仓门禁 2285 Rust 测试 + webui 152 vitest + vscode tsc + grep 零残留。

以下为 **PR 后润色项**（不阻塞）：
1. Models/ThreadsUpdated/Commands 列表通道 v1 note → Host 帧迁移；
2. `StreamFrame::Entry` 信封补齐（id/parentId/timestamp，客户端合成 key 的抖动根因）；
3. steer→parked-submit 的 message_id 关联语义与内核对齐一次；
4. `GetConversationInfo` 的 git 字段（host git 查询）。

原盘点（历史）：

**拆除**：translate.rs 的 4 处 DOOMED note 发射臂；agent_server.rs 的 13 处 DOOMED 引用（含 `republish_if_first_interaction` ×5 与 GetUsage/GetCurrentModel 的 dispatch 臂）；client_store.rs 的 v1 `apply_server_note`（先翻 `stream_drives_render=true` 验证渲染，再删 v1 fold 与 `server_note_translate`）；protocol 的 DOOMED ServerNote 变体 + 守卫/绑定/fixtures 再生成；grep 门禁（§J.6）终检。

**集成复核（T6/T7 交付时上报的事项）**：
1. `StreamFrame::Entry` 信封补齐（id/parentId/timestamp，T7 报的 React key 抖动根因）——协议+follow.rs+两端解析器一次改齐；
2. steer→parked-submit 的 message_id 关联语义（§D.2 vs 无 DropQueued）与 `turnFinish.strandedSteerIds` 的客户端匹配——与 server 对一次；
3. 重连 `StreamEnd{Closed}` 旧代竞态——T7 已用每代轮换 streamId 缓解，T9 同方案，或服务端 re-seat 不重放旧 Closed；
4. `GetConversationInfo` 的 git 字段仍为 null（host git 查询，可选补）。

### K.6 显式不做（本迭代范围外，架构已预留钩子）
动态 host 插件（WASM/JS eval 沙箱）、插件市场与分发、皮肤 token 体系完整化、桌面动态 slot、terminal/ModelChat 域并入 journal。
