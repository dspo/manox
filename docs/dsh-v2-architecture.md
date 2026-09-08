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
- **L10 重放等于内存**：`Thread` 全部可观测状态可由 journal 重放确定性重建（回放一致性测试是内核合并门禁）。facade「镜像内核状态」的双份数据在迁移完成后消灭。as-built 注（K5）：受理条目的 payload timestamp = 受理时刻，run 内 harness 构造的内存副本与之有毫秒级差异——重放一致性以 journal 为准，timestamp 不作逐字节断言面。
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
- 条目行：`{"seq":u64,"id":uuid,"parentId":uuid,"timestamp":iso,"type":snake_case,...payload}`（as-built 修订：on-disk `type` 标签是 snake_case——`turn_start` 等，随 TS Pi v3 schema；**wire** `JournalWireEvent` 的标签是 camelCase——`turnStart` 等，由 translate 层重命名；载荷字段名两侧均 camelCase）。**信封键独占规则**：`seq/id/parentId/timestamp/type` 为信封保留键，事件载荷不得使用同名键（`#[serde(flatten)]` 下同名会互抢/产生重复键）——tool 事件的句柄叫 `callId`，subagent 事件的句柄叫 `agentId`。
- **seq = 活动链深度**：链上稠密 0-based。分叉共享前缀 seq、新后缀续编号；中插（merged follow-up）= 新链 + `leaf` 重定向（现状语义 + seq）。加载时沿 leaf 链校验稠密，违例报错。
- **v3 兼容**：旧文件（version:3、无 seq）读入时按链深回填 seq，内存使用；下次 append 时以 v4 写出（懒迁移）。`leaf.targetId` 游标重定向语义不变。

### C.2 条目词汇表（kernel `JournalEntry` enum，serde tag="type"、tag 值 snake_case；wire `JournalWireEvent` 同名条目、tag 值 camelCase，translate 层一一映射且全射）

| 组 | 条目 | 载荷要点 |
|---|---|---|
| transcript | `message` | user/assistant/tool 消息；assistant 携带 `usage`（input/output/cacheRead/cacheWrite/reasoning）；条目携带 `origin?`（乐观回显退休；**as-built**：Submit 的 origin_rpc 经 `SessionCmd::Prompt` → `Session::set_pending_user_origin` → 持久化中间件在本 turn 首个 user 消息落盘时一次性消费，`append_message_with_origin` 钉入条目；**K5 as-built**：user 条目两个持久时机——direct Submit 在网关受理时 durable 落盘（`persist_user_submission`，accepted⟹logged：receipt 跟条目落地走，持久失败拒绝 receipt；网关跳过条件=残留 pending 会使合并 prompt 偏离本文本），或 queued Submit 在 actor drain 先于 run 持久（合并单条目）；middleware 经 `Session.accepted_user_entry`（entry id+序列化 content 精确匹配）one-shot 跳过重复 append（内容匹配防 next_turn/steer 的 user 行被误跳；**K5-edge as-built**：匹配内容=POST-expansion——受理侧 `persist_user_submission`（resources 随 appender 在 build/swap 发布）与 drain 侧 `persist_prompt_user_entry` 均经自由函数 `expand_prompt_with` 展开（幂等：展开文本无 `/` 前缀），受理条目/pin/run 公告三方同形，slash 命令不再双条目；run 时 input-hook 改写为残余边缘——失配双条目可接受：raw pin=用户意图、追加公告=模型可见真相），jsonl duplicate-id 拒绝仅为结构防线。原设计「消息 id 作条目 id」修订为 pinned(entry_id, content) 槽——AgentMessage::User 无 id 字段，加字段会穿透 provider 序列化与 C.1 信封键独占） |
| transcript | `ui_note` | **K9 as-built 决断**：UI note 走 **custom 面**（`append_custom(UI_NOTE_CUSTOM_TYPE)` → `custom{customType,data}` 条目，display 投影消费 custom 面）；专用 `UiNote` 内核变体与 wire `uiNote` 标签保留而无生产发射——词汇稳定性保留（L12：删=破坏性），是否回收属 C4 时代表面决断。追加失败已与 K4 对称（有界重试+损失记录+facade 通知；无 mid-run 取消腿——UI note 非转录） |

> **K3 as-built 词汇注**：`permission_mode_change` 的 mode 值为 kebab wire 词汇（`PermissionMode::wire`）；早期 tap 映射曾写 Debug 名（如 `"ReadOnly"`），已修——存量 Debug 名条目重放 fail-soft 跳过（保留前值/sidecar 回退），不改写历史行。`approval{kind:"decision"}` 的 verdict 封闭词汇：`allow_once | deny | answered | cancelled`（pending_auth 投影 decision→remove 折叠的真实源）。`pinned_archived` 条目双旗成对（一条即完整重建旗对）；no-op 决策不落条目。
| lifecycle | `turn_start` / `turn_finish{cancelled,failed,strandedSteerIds}` / `stop{reason}` / `retry{attempt,maxAttempts,delaySecs,reason}` / `error{message}` | `anyhow::Error` 过线/落盘转 `{message}` |
| 流式 delta | `agent_text_delta{delta}` / `agent_thinking_delta{delta}` / `tool_call{callId,name,title,status,input}` / `tool_result{callId,output,isError}` / `tool_output_chunk{callId,chunk}` / `subagent_child{agentId,event}` / `subagent_progress{agentId,...}`（≥500ms 或状态变化才记） | dsh chunk 全落盘同款；分页读取端可做 chunk-run 打包（优化，不改语义）；`callId`/`agentId` 遵守 §C.1 信封键独占规则（**as-built**：kernel 侧字段名为 `delta`，wire 映射在 translate 层改名 `s`→`delta` 或直接沿用，见 T4 报告） |
| 状态变更 | `model_change{from?,to}`（to=canonical）/ `cwd_change{cwd}`（**as-built**：盘上字段名 `cwd` 沿用 v3；wire 侧 `cwdChange{path}` 由 translate 重命名——双面字段名以 translate 映射为单源）/ `project_change{path?}` / `permission_mode_change{mode}` / `reasoning_effort_change{effort}`（**as-built**：复用既有 `thinking_level_change` 条目，字段 `thinking_level`）/ `plan_mode_change{enabled}` / `plan_update{snapshot}` / `goal{goal?}` / `title{title}` / `browser_suites{suites}` / `background_task{snapshot}` / `approval{kind:request|decision, authId, payload}` / `pinned_archived{pinned,archived}` | approval request+decision 双态入日志，投影 `pending_auth` 的 fold 源 |
| 压缩/树 | `compaction{...}` / `compaction_started{tokensBefore}` / `branch_summary` / `label` / `session_info` / `active_tools_change` / `custom` / `custom_message` / `leaf{targetId}` | 沿用现 SessionTreeEntry 语义 |
| metrics | `metrics{metricType,data}` | 诊断面；入日志但可声明「低优先」 |

**供 T6/T7 的 as-built 摘要**：宿主侧 journal 供给 = `ThreadHandle::subscribe_journal_feed()`（`JournalFeed::{Event{seq,entry}, Lagged}`）+ `ThreadHandle::journal_snapshot()`（整链冷读）；投影 = `crates/manox-session-core/src/projections.rs::ProjectionSet`（seed+apply+drain_changed/baseline）；waterfall = `src/waterfall.rs::Waterfall`（reply/expire/cancelled_recipients）；origin = message 条目 `origin` 字段（wire 名 `origin`）。

**不成为条目的**（快照边界语义承载）：`HistoryProgress/HistoryRestored` → Snapshot 帧边界；`compaction_started` → 并入 `compaction` 的前置状态或独立条目（选独立条目，UI spinner 需要）。

### C.3 内核改造
- `ThreadCore { state: Thread, journal: SessionLog, subscribers: Vec<Sender<Arc<JournalEvent>>> }`；`JournalEvent{seq, entry}`。
- **as-built 出口拓扑**（修订原文的「出口三段/泵唯一订阅者」）：`with_mut` 出口为锁内收集 `pending_events` → 解锁广播 `ThreadEvent`（内部面，unbounded）；durable 面经 notice tap 汇入 engine actor 队列。**seq 唯一盖章点在 storage 的 append 锁**（jsonl.rs `append_entry_locked`：父必先入索引否则拒绝、重复 id 拒绝、锁内按序广播 `JournalEvent`），facade 状态变更与日志条目为最终一致（tap 滞后 run 调度粒度）。**K6 关闭论证**：跨面定序的四个保证面各有归属——seq 定序=append 锁内单点盖章（结构）；`(cursor, records)` 不撕裂=`reply_journal_snapshot` 单次锁读派生（结构，stall 修复的一部分）；settle 排空=drive_run 返回前排空 serializer（`parallel_tool_rounds_land_every_result` 钉）；退役排空=`retire_and_claim_journal_rows` 同锁 claim（`store_journal_rows_route_to_the_actor_and_shutdown_claim_lands_them` 钉）。tap 滞后窗口本身=最终一致设计，非逐事件定序契约。journal feed 为 bounded broadcast（4096，单源化到 `ENTRY_BACKPRESSURE_CAPACITY` 属 C6 代码项），慢订阅者收 `Lagged` → follow 流以 `StreamEnd{Resync}` 收口（L5，永不静默丢）。订阅者非唯一：每条 follow 流直接订阅 feed，网关泵另订 ThreadEvent 面做裁决路由与簿记。
- `ThreadEvent`（30 变体）保留为内核内部事件面；新增 `ThreadEvent → JournalEntry` 的序列化映射与 `JournalEntry → ThreadEvent` 反投影（桌面视图复用）。新 durable 事件（ui_note/approval/project_change/pinned_archived/title...）直接产生条目。
- 读 API：`journal.cursor() -> u64`、`journal.slice(from..to) -> Vec<JournalEvent>`、`journal.replay() -> Thread`（L10 门禁）。compaction 后 `slice` 的 records 视图从 `firstKeptEntryId` 起（seq 连续性不变）。
- 写放大对策：组提交（批量 flush，默认不逐条 fsync）；页读 chunk-run 打包。唯一允许的回退是 `subagent_progress` 降频，不得回退「条目皆可重放」。
- 整文件重写原子性（K7）：懒 v3→v4 迁移与 deferred 物化一律经 sibling `.jsonl.tmp` 原子替换（write→fsync→rename→best-effort 目录 fsync）：崩溃或并发读者（侧栏扫描、生态工具、follow 冷读）只见完整旧文件或完整新文件，永不见截断中间态；`.tmp` 后缀不入会话目录扫描。
- **durable append 面（K5/K4 as-built）**：storage trait 增 `append_entry_durable`（Jsonl 实现强制 deferred 物化：header 重写 + 已缓冲行 + 本行原子落盘），Session 增 `append_message_durable`；deferred 物化触发 = 首条 assistant 消息（TS parity 不变）**或任一 durable 标记的 append**；受理过 Submit 的 session 视为已交互、非 zombie。typed-append 写面统一 fail-loud（K4）：有界重试（3 次 × 50ms×attempt 退避）→ 永久失败 = durable `error` 条目记录丢失 kind 与原因（storage 自身 down 时 park 进 pending_journal，settle/idle drain 重试，恢复后可见；`error`-kind 行永不自补偿——断 tap 反馈环）+ facade `ThreadEvent::Error` 通知 + mid-run fail-closed（serializer abort 折进 abort_requested，settle 报 cancelled）。对照面（K9 已全部对称化）：middleware 的 message-append 有界重试（3×50ms×attempt）后仍失败才 abort run（消息还原为未发送）；`persist_ui_note` 同款重试+损失记录+facade Error 通知（无取消腿——UI note 非转录，丢失不作废 turn）。
- **K2 as-built（权威迁移）**：`title/pinned/archived/project/permission_mode/reasoning_effort/plan_mode/plan_snapshot/goal/cwd` 的重建权威 = journal（`replay_thread_state` 对 37 变体**穷举 match** 折叠，新词汇必须分类才能编译；last-wins，不可解析词汇 fail-soft 不清旧值）+ `merge_restored_state`（sidecar 只补链上从未出现的字段=懒迁移窗口）+ 背离时缓存收敛修复。三个 restore 面（startup/Open 交换/NewSession）同一实现。title 的缓存修复在**桌面改名接入日志面之前停用**（改名直写 sidecar，修复会回滚用户决策）。goal 权威暂留 threads.db（GoalBridge）；plan_file/plan_review_pending 无条目词汇、留 sidecar。K3 行路由：活引擎行经 ENGINE_ROUTES 入 actor serializer；退役/无路由经有界等待后冷追加（仅当 journal 文件存在）；actor 出口在同锁内 claim 队列——不丢行、不双写为结构性保证。

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
`Initialize{client_id, capabilities: Vec<HookKind>, sessions: Vec<String>, protocol_epoch: u32}`（C1 as-built：服务端接受 epoch 0——v1 serde 缺省——与 `PROTOCOL_EPOCH`，其余以 `protocol/unsupported-epoch` 拒绝且不回 Ready；握手第三帧为 Host `Ready{epoch}` 回显）；`CreateSession{cwd, project?, initial_model?, approval_mode?, reasoning_effort?}`→`{session_id}`（服务端走 `new_in_project` 等价路径；对已存在 session 幂等返回既有 id）；`OpenSession{session_id}`→receipt（历史经 follow 流到达）；`Submit{session_id, text, images?, origin_rpc}`→`{accepted, message_id?}`；`Steer{session_id, message_id, text?, origin_rpc}`→receipt；`PageHistory{session_id, through_seq: i64(-1=最新), before_seq?, max_messages?}`→`{records, has_more, cursor}`（冷读不激活 engine，jsonl 直读；GW6 as-built 三级：活 engine seam → 磁盘冷读（harness 公共读面 `JsonlSessionStorage::open`+`journal_range`，纯读）→ 全新 deferred 会话空页 cursor 0；既无活会话又无文件仍答 `session/not-found`——冷读不遮蔽发现性错误）。**K8 as-built 决议**：①冷读全链解析是有意的——修复页契约(U7)=无界全链+绝对稠密 seq，有界尾部读在此契约下不健全：分支残行使文件行数≠链长（`journal_range` 从 leaf 沿 parent 走链、忽略离链行），而稠密 seq 需要链全长=全链走；真有界冷读需链长索引/sidecar 前置（更大设计，缓议）。②group commit 已是默认：`append_line` 无逐行 fsync（O_APPEND 单行写），fsync 仅在 K7 原子重写路径（temp+sync_all+rename+目录 fsync）。③compaction 感知切片由 K7 重写自然成立：重写后文件=summary+保留尾，`journal_range` 供给的即 compaction 后链，seq 稠密不变（§C.2 既有承诺）；`GetConversationInfo{session_id}`→fold 载荷（§E.3；按 `(thread_id,cursor)` 缓存）；`ListThreads`/`ListModels`/`ListCommands`→快照（纯读，允许领域数据——L7 只约束写）；`TerminalAttach/TerminalSnapshot/ModelChat*`（沿旧）；`CancelDelivery{delivery_id}`→`{cancelled}`（GW3：撤回未决裁决投递；已 settle/非 recipient 答 `cancelled:false`，恒回 receipt——L7）。

### D.3 ClientNote（fire-and-forget 命令）
`DetachSession, DisposeSession, DropQueued, CancelTurn, SetModel{session_id, id}, SetReasoningEffort, SetApprovalMode, SetCwd, SetPlanMode, SetBrowserSuite（U6b① 增补：suite 为封闭 wire 名 "chromeuse"|"webexplore"，效果经 BrowserSuitesChanged 回声返回）, PlanSeedExecution, Compact, Goal, StopBackgroundTask, ArchiveThread, PinThread, TerminalInput, TerminalResize, CancelModelChat, Shutdown, AppendUserMessage, AppendUiNote`，加 compat 三件 `CreateSession, Submit, Steer`（C4a-2 验证：桌面/webui 生产发送方已清零，仅为 vscode extension host 存活；删除 gated 于 vscode 迁移）——共 25 个（U6b① 增 SetBrowserSuite；FocusThread 已于 C4b-narrow 删除：GW5 起 handler no-op、无发送方，vscode 死帧行同批退休），以 `CLIENT_NOTES` 宏生成清单为单源。**C5 as-built**：`SetModel` 字段为 `id: String`（canonical ModelRef 串）；原规格的 `model: ModelRef` 类型化字段属 C4 时代的 wire 收敛（改字段名=破坏性变更）。

### D.4 ServerCall（waterfall，§K 见 J7 语义）
`Approve{delivery_id, auth_id, tool_name, summary, input}`；`AskUserQuestion{delivery_id, ...}`；`PlanVerdict{delivery_id, plan_file, title, body}`——三者 fan-out 给 `owner(session) ∩ capability` 全部连接，全部 `next` 才放行、任一 `rejected` 即取消并广播 cancel；单 owner 行为与现状一致。`BrowserOp/ClipboardRead/OpenExternal` 定向单连接（owner∩capability 首个，超时作废按 rejected）。GW3 as-built：delivery_id 由 route_call **单点盖章**（`dlv-{session}-{n}`，一次 fan-out 全员同 id；投影/泵构造点携空占位、不铸身份）；`CancelDelivery` 的 per-recipient token 把取消折成 `expired` 进 funnel——与超时同路，复用既有 fail-closed/converge 收敛，无平行取消语义；撤回后的晚到 Reply 被忽略；投递注册表 settle 注销（Drop 守卫覆盖泵 abort）。

### D.5 HostEvent（全局通知，取代 server 端领域 note）
`Ready{epoch}`；`Models(Vec<ModelInfo>)`（**U2-跨域#2 as-built**：provider reload 即推已实现——`provider_glue` 单槽 reload 监听器（AgentServer 构造注册，弱引用、后注册胜），`reload()` 换出快照后在注册表写锁外触发 → `broadcast_models_after_reload` 全连接广播；仅 Host 帧——非请自的 Models 推送无 v1 note 消费方（两端均已折叠 HostEvent::Models），ListModels 响应面维持 GW1 双发。桌面 settings 的进程内 reload 调用点不变）；`Commands(...)`；`ThreadsUpdated(Vec<ThreadSummary>)`（元数据变更时全量快照：title/pin/archive/model/分组。**U6a as-built：变更触发器=服务端 store 事件 watcher**——AgentServer 构造 spawn,订阅 store `subscribe()` 通道(非 gpui,网关可持有),SummariesUpdated → 全连接广播 note+host+Projects(爆发合并,每 drain 批一次;RunningChanged 不广播——running 列骑 SessionStatus delta);弱引用 inner,store 通道关闭即退休;测试经 `new_without_store_watcher` 关断(严格帧序列确定性,K.7.1)。**U6a 前无主动触发点**(push 臂只挂 ListThreads 响应),列表刷新靠桌面 store 事件桥 refetch——桥已退休）；`SessionStatus{session_id, running?, errored?, unread?, pending_auth?, pending_plan?, background_work?}`（小 delta 高频；turn 生命周期/审批挂起/后台任务时广播给**全部**连接；GW5 as-built：settle 恒发 `unread:true`、服务端永不降也不再有 focused 镜像（FocusThread 变体已于 C4b-narrow 删除），清零是**客户端本地**行为——桌面 leaf active 门控 / webui `!active` 门控；客户端单调镜像：unread 只增至本地清零、errored 边沿置位、running 最新为准。**U3b as-built：裁决时刻清零归服务端**——pending_auth 在全部四个裁决落点（allow/deny、ask 回答、双 fail-closed）经 `clear_pending_auth_if_settled` 清零，真值=facade pending 集（最后一个授权 settle 才降旗，并发授权保持）；pending_plan 在 refine/execute 两臂经 `clear_pending_plan_flags` 清零（reject/expire 走 GW9 收敛）；Error 边沿 `mark_idle`+全徽章清零且**单帧 delta 携带全套**（running:false/errored:true/plan:false/auth:false/background:false）。桌面端对应启发式清除（前台泵/parked 启发式/裁决本地清）为冗余，随列表迁移后的 agent-ui 批删除）；`SessionCreated{session_id, header}`/`SessionDisposed{session_id}`（owner 集合内控制）；`Error{message}`。过渡期**双发**（GW1 as-built）：每个 v1 ServerNote 发射点在同受众镜像 Host 帧（broadcast_host 全局 / route_host owner 集 / host_to_client 定向；note 先、host 后——v1 客户端帧前缀逐帧不变），客户端双轨折叠须幂等；C4 删 note 臂。HostEvent::Error 自 **C4a** 携 `session_id: Option<String>`（serde-default 且恒序列化，null=连接级、无 leaf 归属；v1 note 的作用域平价——权威翻转后桌面 leaf 规范化按它过滤），受众仍为 owner 集、不全局广播。

### D.6 死亡清单（迁移完成后删除）
`ServerNote::{AgentText, AgentThinking, ToolCall, ToolResult, ToolOutput, TurnStarted, TurnFinished, Stop, Retry, Compaction*, Subagent*, ModelText/Thinking/ToolCall/ModelChatDone, ThreadInfo, ThreadHistory, ThreadsUpdated, Models, Commands, Usage, UsageSnapshot, TokenUsage, CurrentModel, PlanReady?, PlanUpdated, PlanModeChanged, GoalChanged, CwdChanged, PermissionModeChanged, ReasoningEffortChanged, BrowserSuitesChanged, BackgroundTaskUpdated, SteerPending/Injected, ApprovalDecision, Branch, GitStats, HistoryProgress, PeerMessage?, CacheInvalidation, Error}`——分别由 Entry 条目 / 投影 / HostEvent / Snapshot 边界取代。`translate.rs` 的镜像臂全灭，ServerCall 生成臂保留迁入新泵。

**as-built（arch 审计修订）**：拆除实际删 37 留 11（保留集＝owner 控制 `ready/sessionCreated/sessionDisposed`、过渡列表通道 `threadsUpdated/models/commands`、服务端 `error`、ModelChat 侧流 ×4——以 surface.rs 的 `SERVER_NOTES` 宏生成清单为准）。§J.6 的「零残留」声明不实：compat `ClientNote::{CreateSession,Submit,Steer}` 仅为 vscode extension host 存活（C4a-2 验证：桌面 landing 已走 v2 `ClientCall::CreateSession`、webui 三处均 v2 request）；错误桩 `ClientCall::{GetUsage,GetCurrentModel,ThreadInfo}` 已于 C4b-narrow 删除（零发送方，grep 证据）。C4 关闭双协议窗口时经 surface 宏清单一次删除（穷举 tag match 使表/样本/类型同步收敛，编译期门禁）。GW1 已落地：8 个 HostEvent 变体全部有生产发射点（与 v1 note 同受众双发）。客户端迁移：webui store 折叠 Host 镜像✓；**桌面 U2 已落地**——multiplexer `apply_host` 全量镜像（Ready{epoch} 校验 PROTOCOL_EPOCH+触发 ListThreads/ListModels/ListCommands 首拉、Models/Commands/ThreadsUpdated 替换、SessionStatus 按 §D.5 客户端单调规则逐条对齐 webui、SessionCreated/Disposed/Error 权威已翻转（C4a-1：host SessionCreated 臂承担 leaf 创建+has_follow 守卫的 follow 首开，leaf Host 臂将三类控制帧规范化入 note 路径，note 臂仅对账；HostEvent::Error 携 session_id 作用域）），sidebar/列表面零内核读✓；C4b 大宗删臂 gated 于 vscode host 迁移（follow-up）。

### D.7 背压与错误
- 策略表：`StreamItem(Snapshot|Projections)` 与 `StreamEnd` → 永不 Drop；`Entry` → 有界（4096，单源常量 `ENTRY_BACKPRESSURE_CAPACITY`）满即发 `StreamEnd{Resync}`；控制帧（Request/Response/Reply/Host）→ 阻塞不丢。
- 载体分界（C6 决断，round-10 死锁教训）：**网络载体**（WS，有界 1024）执行上表——有界、阻塞、满即 Resync；**进程内载体**（GPUI/napi 的 in-proc pair）按设计无界：两端同进程，有界对在双向同时填满时只会让双方 park 在 `send_blocking` 上互等而死锁（round-10 的 GPUI 主线程冻结即此），L5 的有界-重同步语义只适用网络载体，慢客户端在进程内的代价是内存而非丢帧；任何发送方不得在 GPUI 主线程上同步阻塞。ModelChat 侧流 note（modelText/modelThinking）为 §B L6 域旁路，明文声明为可损（Drop 类保留）。
- 服务端广播纪律（GW4）：`broadcast_host`/`note_to_client` 一律锁内 clone 连接列表、锁外发送——单个停滞的网络客户端不得持 `clients` 锁冻结全网关（克隆后发送，照 `route_note` 范本）。
- `RpcError{code, message}`，code 集：`session/not-found, session/busy, gateway/bad-request, gateway/internal, resync-required, model/unresolvable, feature/unavailable, protocol/unsupported-epoch`（C1：epoch ∉ {0, PROTOCOL_EPOCH}）（GW7 增补：协议已声明但尚未实现的能力臂以此作答，客户端可区分「功能未建」与一般失败；Terminal 死桩为首个使用者）。**C2 错误码纪律**：离开网关的每个 `RpcError` 都携稳定码（生产区零无码——结构门禁 `every_production_rpc_error_carries_a_stable_code` 扫描构造点，构造必须立即链 `.with_code`）；成功载荷的类型化 RpcOutcome 为 Wave 2 项（波及全客户端 Response 消费面，与 C4 wire 工作同批）。

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
1. 打开：首帧必为 `Snapshot`，校验页内条目互相邻接、页尾=cursor；发布 `Replace{records, projections}`。（**cursor 语义 as-built**：wire `cursor` = 条目计数（dense 0-based ⇒ 排他端）；本规则的「页尾=cursor」指 inclusive 页尾 seq——非空窗口等于最后一条 record 的 seq，空快照保留 wire cursor（emptyCursor case）；两端在快照消费点显式换算。）
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
2. 回放一致性（L10）：落盘重载 == 内存（display/投影/游标）。**K1 as-built**：门禁 `journal_replay_is_consistent_across_disk_reload` 覆盖 30 种 on-disk kind（REPLAY_COVERAGE_KINDS 对链断言防空转），经生产 `builder.open` 重载路径逐面相等。两个 as-built 非逐字节面（测试内注明）：消息载荷 timestamp（K5 受理/运行毫秒差）与 **display message id**（`entries_to_display` 每次重建新铸 UUID、非 journal 派生——候选后续：display id 改由条目 id 派生以获得 UI key 稳定性，T6/T7 关联）。
3. JournalStream 属性测试（F.1.5）。
4. 声明面覆盖（L12）：journal 条目/投影 key/host 事件/协议帧四张表，emit 点 100%（coverage 测试：脚本化会话驱动后断言每个声明面出现在 FromServer 流；扩展 `dual_path_transport_consistency`）。
5. 双路径一致性：in-proc ≡ serde ≡ TS 守卫（fixtures）。
6. grep 门禁（终局）：视图/组件层不得 import 协议发送面；死亡清单残留以 §D.6 as-built 保留集为准（原「零残留」声明经审计证伪），并按 U9 扩展到内核对象面（views 不得持 ThreadHandle）。
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

**终局状态**：translate 只余裁决路由；桌面渲染与 restore 全走 v2 流（含重开回归锁 `reopen_snapshot_restores_transcript_and_rearms_rebuild`）；v1 快照发射面（ThreadHistory/ThreadInfo/GetUsage/GetCurrentModel/残余 PermissionModeChanged/SteerPending）删除；37 个 DOOMED ServerNote 变体删除（保留 11 个：owner 控制 ×3、过渡列表通道 ×3、服务端 Error、ModelChat 侧流 ×4）；双端 v1 fold 清除；绑定/守卫/fixtures 再生成；桌面 usage 面板接 Q 面（committed 边沿，含回归测试）；全仓门禁 2285 Rust 测试 + webui 152 vitest + vscode tsc（「grep 零残留」声明经 arch 审计证伪——真实保留集见 §D.6 as-built 注记；且 §J.4 发射覆盖门禁当时为自指样本，C3 已改为宏单源+穷举 match，真实组合发射覆盖属 J1b）。

以下为 **PR 后润色项**（arch 审计后更新状态与范围）：
1. HostEvent 总线迁移（GW1，open）——审计修正范围：8 个 HostEvent 变体中仅 `SessionStatus` 有生产发射点，`Ready/Models/Commands/ThreadsUpdated/SessionCreated/SessionDisposed/Error` 全由 v1 ServerNote 承载；`pending_plan`/`background_work` 的 SessionStatus delta（含 true 边沿）已补齐；
2. `StreamFrame::Entry` 信封补齐——**已关闭（U5）**：帧携完整 §C.1 信封（seq/id/parentId/timestamp/event），follow.rs 与快照 records 走同一 `wire_entry` 变换（live 帧与其快照孪生同形），桌面 fold 与 webui store 双双退休合成 `e-{seq}` id，TS 边界（guards.parseStreamFrame）以 exactKeys 钉住信封；
3. steer→parked-submit 的 message_id 关联语义与内核对齐（GW8，open）；
4. `GetConversationInfo` 的 git 字段（open）。

原盘点（历史）：

**拆除**：translate.rs 的 4 处 DOOMED note 发射臂；agent_server.rs 的 13 处 DOOMED 引用（含 `republish_if_first_interaction` ×5 与 GetUsage/GetCurrentModel 的 dispatch 臂）；client_store.rs 的 v1 `apply_server_note`（先翻 `stream_drives_render=true` 验证渲染，再删 v1 fold 与 `server_note_translate`）；protocol 的 DOOMED ServerNote 变体 + 守卫/绑定/fixtures 再生成；grep 门禁（§J.6）终检。

**集成复核（T6/T7 交付时上报的事项）**：
1. `StreamFrame::Entry` 信封补齐（id/parentId/timestamp，T7 报的 React key 抖动根因）——**已关闭（U5）**：协议+follow.rs+两端解析器一次改齐；
2. steer→parked-submit 的 message_id 关联语义（§D.2 vs 无 DropQueued）与 `turnFinish.strandedSteerIds` 的客户端匹配——与 server 对一次；
3. 重连 `StreamEnd{Closed}` 旧代竞态——**已关闭（arch 审计验证）**：服务端 re-seat 先 `disconnect()` 旧连接再 end 旧流（旧 Closed 只发往死通道），`remove_client` 有代际栅栏、follow untrack 有身份栅栏；客户端每代轮换 streamId 为双保险；
4. `GetConversationInfo` 的 git 字段仍为 null（host git 查询，可选补）。

### K.6 显式不做（本迭代范围外，架构已预留钩子）
动态 host 插件（WASM/JS eval 沙箱）、插件市场与分发、皮肤 token 体系完整化、桌面动态 slot、terminal/ModelChat 域并入 journal。


### K.7 arch 审计整改波（arch/dsh-v2 第二波，进行中）

四路只读审计（协议/网关/内核/桌面）+ 主线交叉验证产出 spec 级问题清单（编号 K*/C*/GW*/U*/J*），按 Wave 0（止血：数据丢失/瘫痪/冻结）→ Wave 1（架构承诺收口）→ Wave 2（契约完成与债务）实施；纪律：每项 = 规格修订 + 实现 + 回归测试 + 红前绿后证明（对旧实现临时回退必须报红）。已提交：GW4 广播锁外发送（cc813c42）、K7 整文件重写原子替换（2ffaed80）、GW11 冷 id CreateSession 恢复而非重铸+套件卫生（56a27ae1）、C3 声明面宏单源+编译期穷举门禁+TS 同步断言（82eeeba2）、U7 Q 面增量计数+120ms 去抖（63a58396）、GW2/GW9/GW10/GW7 网关生命周期批次（b3409d18）、§D.5 双边沿+detach 延迟回收+规格真实化（037d5d2e）、U5 Entry 帧信封补齐（93876496）、round-11 turn-stall 根因修复+K4 typed-append fail-loud+K5 受理即持久机制面（4ac63866）、K5 网关接线（c7cea383）、GW5 客户端半边：unread 客户端拥有+侧栏徽章读 leaf 镜像（768fc7ec）、U3a 九处冗余 store 镜像写删除+棘轮 27→18（2c6ad9d4）、C1/GW1/GW3/GW5-服务端/GW6 网关批次（c12309e8）、webui unread 客户端拥有收尾（49cfaae4）、C2 错误码纪律+结构门禁（aed45a57）、J1 真实组合发射门禁·host+call 面（898b9ee3）、K3 决策点入日志+K2 journal 重建权威+K1 L10 重放门禁（f51dee3f，approval 裁决词汇 allow_once|deny|answered|cancelled、ENGINE_ROUTES 冷追加/退役 claim 协议）。round-11 turn-stall 已根因定位（drive_run 的 select 同任务自死锁：AppendJournal 臂内联等 append_lock，同 select 的 run 分支持锁挂在文件 IO；修复=AppendJournal 转发专用 serializer 任务+快照读同锁派生 cursor），验收提交中。


### K.7.1 事故 ↔ 回归对照表(J1 门禁的可审计面)

整改波每个事故类必须有一个具名回归测试钉住;本表是对照单一事实源。门禁语义:表中测试名必须全部存在于套件(grep 可验),且各自附带红前绿后证据(对旧实现临时回退报红)。**新增事故类必须新增一行**——没有回归测试的事故修复不算完成。J1 的"真实组合发射覆盖门禁"(§J.4)在此之上再加一层:声明面每个条目/事件/帧必须在真实网关组合中被发射过一次(而非自指样本)。

| 项 | 事故类 | 回归测试(crate) |
| --- | --- | --- |
| GW4 | 单个停滞客户端持 `clients` 锁 → 全网关冻结 | `stalled_host_broadcast_never_holds_the_clients_lock`(session-core) |
| K7 | 整文件重写非原子 → 并发读者见截断中间态 | `concurrent_reader_never_observes_a_torn_rewrite`、`failed_rewrite_leaves_the_original_file_intact`(harness) |
| GW11 | 冷 id CreateSession 重铸而非恢复 → 历史丢失 | `create_session_with_a_cold_persisted_id_restores_history`(session-core) |
| C3 | 声明面表/样本/tag 三处手维护漂移(FRAMES 缺 followSession、假 Response 信封) | `tag_functions_agree_with_tables`、`frames_is_the_mechanical_concatenation`、`export_surface_tags`(protocol)+ webui `guards-surface.test.ts` |
| U7 | Q 面每帧 O(window) 全扫 + 无去抖 | `q_face_committed_counter_is_incremental_and_exact`(agent-ui) |
| GW2 | 泵永生 → dispose 后重开双泵 → 审批被自动拒绝 | `dispose_then_reopen_keeps_exactly_one_pump`、`concurrent_open_session_yields_one_entry_one_pump`、`rpc_peer_duplicate_register_keeps_the_first_waiter_alive`(session-core/protocol) |
| GW9 | PlanVerdict 被拒/超时 → pending_plan 永久卡死 | `plan_verdict_rejection_converges_pending_state`、`plan_verdict_without_reviewer_converges`、`converge_plan_rejected_clears_every_plane_directly`(session-core) |
| GW10 | 重握手 owner 注册非幂等 → 重复帧 + MsgId 自撞 | `rehandshake_same_client_id_keeps_one_owner_row_per_session`(session-core) |
| GW7 | Terminal 死桩静默吞字节 / 无稳定码 | `terminal_calls_answer_feature_unavailable`、`terminal_notes_answer_with_an_error_note`(session-core) |
| D.5 | pending_plan/background_work 边沿不广播;detach 停泵后 running 旗滞留 | `submit_receipt_waits_for_accept_time_persistence`(true 边沿)、`detach_while_running_defers_reap_until_settle`(session-core) |
| K4 | typed-append 永久失败静默丢行 | `mid_run_typed_append_permanent_failure_fails_loud_and_cancels`(agent) |
| K5 | receipt 先于持久 → 受理后崩溃丢文本;queued 双写 | `accepted_user_entry_persists_before_the_run_and_the_middleware_skips_the_duplicate`、`kill_after_receipt_keeps_the_accepted_entry_and_origin`、`queued_submit_persists_at_drain_before_the_run`、`stale_accepted_pin_never_leaks_into_the_next_turn`、`prompt_pin_carries_the_expanded_text`(agent,K5-edge)+ `submit_receipt_waits_for_accept_time_persistence`、`submit_persistence_failure_refuses_the_receipt`(session-core 网关接线) |
| STALL | round-11 turn 停滞:drive_run select 同任务自死锁 | `parallel_tool_rounds_land_every_result`(agent;曾 ~2/3 红,现 10/10 稳绿) |
| U5 | Entry 帧无信封 → 两端合成 `e-{seq}` id 在 Replace 后漂移 | `entry_and_projection_frames_round_trip`(protocol)+ q_face durable-id 断言(agent-ui)+ webui entry 守卫/桥接测试 |
| U3a | 桌面九处 store 镜像写与泵竞态(单写者违背) | `desktop_bypass_surface_never_grows`、`no_new_files_touch_wire_or_store`(agent-ui 棘轮门禁) |
| GW5 | unread 服务端单槽 focus 镜像无法表达每客户端;正看着的会话也点亮;blur 后徽标永不亮 | `active_leaf_suppresses_unread_and_focus_clears`、`multiplexer_focus_transitions_gate_the_leaf_mirrors`(agent-ui)、`turn_settle_raises_unread_even_after_focus_report`、`list_threads_unread_is_always_false_and_focus_is_noop`(session-core)、`GW5: a list refresh preserves the client-owned unread`、`GW5: backToList is the local blur`(webui) |
| C1 | 握手无版本协商 | `handshake_rejects_unknown_protocol_epoch`、`handshake_accepts_v1_client_without_protocol_epoch`、`handshake_ready_double_emits_host_epoch_echo`、`initialize_protocol_epoch_defaults_to_zero`(protocol/session-core) |
| GW1 | HostEvent 总线 7/8 变体无生产发射点 | `list_pushes_double_emit_host_frames`、`session_created_double_emits_directed_host_frame`、`session_disposed_and_detached_double_emit_host_frames`、`error_notes_double_emit_host_error`(session-core) |
| GW3 | 裁决投递无 delivery_id、不可取消 | `adjudication_requests_carry_stable_delivery_id`、`cancel_delivery_converges_pending_adjudication`(session-core) |
| GW6 | 冷读缺位 → 无 engine 会话 PageHistory 30s 挂起 | `page_history_cold_reads_disk_for_opened_session_without_engine`、`page_history_reads_disk_without_live_session`、`page_history_unknown_session_still_answers_not_found`、`concurrent_create_and_open_same_cold_id_singleflight`(session-core) |
| 测试隔离 | 触达 plugin_hooks::fire 的测试单独跑 panic(runtime 未初始化) | `mid_run_append_ui_note_mirrors_now_and_parks_persist`(agent;自持 runtime::init) |
| 测试隔离 | Approve 能力+TurnStarted 组合的测试在同进程任何先行测试后,首个 running-delta expect 恒 30s 超时(同链无能力的 `detach_while_running` 稳健;有能力无 TurnStarted 的 `approve_verdict_clears…` 亦稳健);根因未定位,疑共享全局态 | `error_edge_idles_store_and_clears_adjudication_flags` 重设计为无能力配置+徽章 store 直写前置;真实裁决流由 approve_verdict 测试独立覆盖(未决:根因) |
| 测试隔离 | gpui 确定性调度 vs ListThreads 自持 rescan:毫秒级应答延迟使 foreign-thread(tokio-rt-worker)唤醒落入 scheduler 活动窗 → navigator_fill/right_pane_state_machine 等报 "not deterministic"(block_in_place 同轮询化亦不愈;teardown 竞态的严格 global() panic 已另行 try_global 容错) | `thread_store::test_override_active()` 时跳过自持扫描:生产(init 无 override)恒扫,session-core 套件(生产 init)以 `list_threads_self_holds_the_rescan` 钉真实自持;gpui 套件回 #5 前时序剖面 |
| 测试隔离 | U6a store watcher 广播 vs 严格帧序列测试:scan/写触发的 ThreadsUpdated 广播污染 add_owner/dual_path/list_threads_pushes/agent_client 等精确序列断言(逐次抖动时序抽奖);桌面套件另有 global-singleton server 错位(watcher 订阅首个构建者的 store,后续测试的 store 写无广播——u2(b) 独跑绿/套件红) | session-core:测试脚手架经 `new_without_store_watcher` 关断 watcher(10+2 构造点),广播行为由专测 `store_change_broadcasts_the_list_refresh`(harness_with_store_watcher+容忍握手)钉住;桌面:u2(b) 显式驱动 fetch(生产由 watcher 广播覆盖,三层测试各钉一段:watcher=专测、mux 折叠=host-mirror 测试、fetch→rows=u2) |


### K.7.2 余债账本(整改波内的认可债务,单一事实源;清偿归 Wave 2/C4)

| 债务 | 现状 | 归属 |
| --- | --- | --- |
| U3b:用户动作写无网关调用(archive/tag/remove_project/register_project 直写 store) | 需协议新 call(与 C4 面一起设计) | C4 |
| U6(**桥已退休✓ U6a**:服务端 store watcher 广播列表刷新——§D.5「元数据变更时推送」由虚转实;桌面 store 订阅/_store_pump 字段/on_thread_store_changed 删除,u2(b) 改显式驱动 wire 腿(desktop 套件 global-singleton server 的 watcher 订阅首个构建者的 store,生产次序无此虞:store init 先于首个 workspace);STORE_GLOBAL 棘轮**持平 14**——thread_store 绑定为 attach 面读点存活)。**余 U6b 实施地图**(普查已定界):workspace `self.thread` 29 处(with_mut×6/read×4/clone×3+散布读)——①browser-suite 开关 **✓ 已落地(U6b①,形状修正:setter NOTE 族而非 call——与 SetPlanMode/SetModel 同族)**:ClientNote::SetBrowserSuite{session_id, suite:封闭 wire 名, enable}+engine BrowserSuite::wire/from_wire+服务端臂(landing 镜像 park/活引擎经 facade→engine cmd;未知 suite 名回 error note 非 panic)+桌面 toggle 迁移(landing 无线可走时 fallback facade park=设计内着陆路径,ensure_engine 重放;棘轮 SENDS 21→23 带理由注记、FACADE_WRITE 持平 5——toggle 主路径已 wire 化,残余=dismiss+2 构造点+2 landing fallback);回归 set_browser_suite_note_lands_on_the_facade+变异红;②open_thread 的 load_thread→landing-mirror 化(restore 走 wire reopen 流,attach_created_session 的 mirror fallback 为先例);③构造点写(4394/6458 with_mut)→CreateSession call 字段(cwd/initial_model/approval_mode 均已在词汇);④dismiss 写(submit 路径消解 plan review——**无服务端覆盖,是消解的唯一持久记录**,与已退休的裁决本地清不同性质)需 wire 语义决策(refine-as-dismiss 或依赖 300s expiry 的 GW9 收敛);⑤background_threads parking(持 Thread entity 的后台续跑/回收)=最深面,attach 生命周期模型重构;⑥直读退休=①-⑤后 live_thread/load_thread 删除+thread_store 句柄收缩 | U6b 分步实施 | Wave 2(多轮) |
| vscode:徽章迁移(GW5 后列表 unread 恒 false;focusThread 死帧行已随 C4b-narrow 删除) | 与 ThreadListItem.unread 列删除同批(webui ThreadRow 类型手术+badge 同形) | C4 |
| C4b 大宗删除 gate:vscode extension host 消费 note 形状事件(sessionCreated×3/threadsUpdated×2/sessionDisposed×2/ready×2/approve×2/planVerdict/askUserQuestion/models/commands——awaitSession 谓词+审批流);parseFromServer 不归一 host 帧 → 删臂=vscode 会话创建挂起(硬破坏,非 badge 类退化) | vscode host 迁移(parseFromServer 把 host 帧归一为 method 事件,或 sessionManager 谓词改 host 形状)为 C4b 大宗+compat 三件删除的硬前置;本期 C4b 仅删无消费方面 | C4 follow-up(硬) |
| C2:无码错误归码 ✓(错误码纪律+结构门禁 every_production_rpc_error_carries_a_stable_code,aed45a57);RpcOutcome 类型化 → **归 C4b 大宗批**(§D.7 as-built 自注:「波及全客户端 Response 消费面,与 C4 wire 工作同批」;vscode host 迁移 gate 同批适用) | C4 | C4 |
| U7b:Q 面 visibility 门控(rail 可见性状态归属) | U9 拆分后做 | U9 |
| U9:workspace.rs(11.7k 行)/agent_server.rs(6.7k 行)拆分 + 三层归属表 + 依赖门禁 | — | U9 |
| C5:字段漂移——文档面已对齐（D.3 清单 24 项(C4b-narrow 后)/SetModel `id` 字段/cwd 双面命名/ui_note 决断/cursor 语义）；SetModel 类型化 ModelRef 字段重命名=破坏性 wire 变更 | C4 时代 wire 收敛 | C4 |
| K9:ui_note 决断✓、cursor 语义注✓（均已入 §C.2/§F.1 as-built）；persist_ui_note 对称化✓（K9 提交）；~~死 schema 回收~~ **C4b-narrow 核验为误诊销项**：K9 对称化后 persist_ui_note 写 typed `ui_note` 条目（"legacy custom 行不再写"），translate→wire `uiNote`→webui/桌面消费链完整=活 schema | 无（回收项撤销） | ✓ 关闭 |
| Ready 快照可能以旧值覆盖加载期 pin 的 facade 镜像（gateway 投影 higher-seq-wins 自愈、桌面读 summary 不受影响） | 已注释级风险，观察 | 观察 |

### K.7.3 C4 收口准备清单（prep 盘点，非实施——执行顺序与前置以本表为准）

**协议删除面（L12 破坏性变更，单批收敛）**：
1. 错误桩 ClientCall 三件：`GetUsage`/`GetCurrentModel`/`ThreadInfo` **✓ 已删（C4b-narrow）**——零发送方（grep 证据），变体+surface 条目+样本+服务端合并桩臂+J1 穷举清单同批收敛。
2. compat ClientNote 三件：`CreateSession`/`Submit`/`Steer`——U1-flush 已落地（parked flush 改道网关，前台 flush 此前已迁）。**C4a-2 验证（零迁移工作）**：三件的生产发送方已清零——桌面 create 走 `ClientCall::CreateSession`（multiplexer `create_session`，open_or_create 的 note 路径仅测试脚手架）、submit 走 `ClientCall::Submit`、steer 桌面无 UI；webui 三处 `method:'submit'/'steer'/'createSession'` 均为 **v2 `request()` call**（method 字符串曾误判为 note 残余）。实际发送方仅 **vscode extension host**（sidebarProvider/participant 的 submit note、sessionManager 的 createSession note——client-chosen-id + awaitSession 流，迁移是行为级改动，超出「仅保持编译」授权）→ 三件服务端臂为 vscode 保留，删除 gated 于 vscode 迁移（follow-up 行）。`AppendUserMessage` 批量前置插入为**合法 v1 note**（不在三件内）：flush 替代的语义决策点仍开放（逐项 Submit 在 idle 线程上首条即开行、余条并第二轮,与现状单轮批量不等价——需批量词汇或接受两轮形状并入档）。
3. `FocusThread` 变体 **✓ 已删（C4b-narrow）**——GW5 起 handler no-op、桌面/webui 零发送；vscode 死帧行同批退休（陈旧帧解析失败即丢弃=与 no-op 等价）；两个 GW5 回归测试迁移（焦点探针随变体退休，settle 升 unread/遗留镜像存活/列表恒 false 断言保留）。
4. `ThreadListItem.unread` 弃用列（GW5 起恒 false，两端均已 delta+本地清零）**→ 重归 C4b 大宗**：webui 以生成的 ThreadListItem 作本地行类型（client-owned unread 骑同字段），删列需 ThreadRow 类型手术——与 vscode badge 迁移同批（同形）。
5. T2 死亡清单 `#[deprecated]` 类型（ServerNote 死亡清单=37 减 11 保留集的差集；保留集以 `SERVER_NOTES` 宏清单为单源）。
6. `UiNote` 内核变体 + `uiNote` wire 标签 **✗ 误诊销项（C4b-narrow 核验）**：K9 对称化已使 persist_ui_note 写 typed `ui_note` 条目（engine 注释："The legacy `custom` row is no longer written"），translate 投影为 wire `uiNote`，webui `case 'uiNote'`/桌面消费链完整——**活 schema，不回收**（本批删除曾短暂触及，编译+persist 路径核验在提交前拦下，harness/replay 复原为净零 diff）。
7. `SetModel.id:String` → 类型化 `ModelRef` 字段（C5 wire 收敛）。
8. plan-verdict 专用条目词汇增补 **✓ 已落地（`plan_review` 条目=词汇第 38）**——`state: "proposed"|"resolved"` 双边沿（verdict 区分值骑 notice 面，条目为投影折叠源；区分值增补记为可选扩展）：actor `SetPlanReviewPending` 臂单点 append（提案/裁决全部经此收口，桌面+服务端调用方零改动）；replay 折叠 `plan_review_pending: Option<bool>`（None=链未见=sidecar 提示站立）；P 面投影 `set("plan_review_pending")`；restore journal-first 合并（sidecar 降 pre-vocabulary hole-fill）；桌面 `thread_event_of` 显式 None（pending 骑 SessionStatus delta）；webui entries default 安全忽略+guards tag 同步（C3/J.5 门禁曾按设计红——fixture 再生后绿）。
9. vscode：focusThread 死帧 **✓ 已删（C4b-narrow，sidebarProvider 死帧行）**；徽章迁移仍 follow-up（与 unread 列删除同批）。

**服务端删除面**：GW1 双发的 v1 note 臂全删（8 个 HostEvent 发射点保留 Host 帧；C3 宏表 SERVER_NOTES 37→11 收敛，穷举 tag match 使类型/表/样本同步）。

**客户端前置（note 臂删除前必须完成）**：
- 桌面：`SessionCreated/Disposed/Error` 权威迁移 **✓ 已落地（C4a-1）**——mux `apply_host` 的 SessionCreated 臂接管 leaf 创建+has_follow 守卫的 follow 首开（note 臂降为对账）；leaf Host 臂将三类控制帧规范化入 note 路径（webui onHostEvent 同款：SessionCreated 绑空 store.id、scoped Error 合成 note 并 emit ThreadEvent::Error、逐 leaf 按会话过滤；GW1 双发幂等）；HostEvent::Error 增 session_id 作用域列。**C4b 删服务端 note 臂的桌面前置已清**。
- 桌面：`model_cascade`✓（改读 mux wire models,provider_glue 直读退休）、ThreadRowMeta 装饰推送✓（sidebar 分组/tag/wash 直读 wire 行,注册表读 mux `known_projects` 镜像,workspace 装饰推送/read_thread_decor 退休,chip 缓存改 mux observe 供给）——跨域#1/#4 全闭;残余仅 `refresh_thread_list` 7 处+store 事件桥 → 服务端 rescan 自持（跨域#5）。
- webui：T7 v2-first✓；**消费残余已审计定界**（`apps/web/webui/src/sidebar/webview/state/store.ts`）：`notification` 路由 1 处（~325）+ `onServerNote` 纯 v1 臂 7 个——注册表 4 臂（models/commands/threadsUpdated/ready，~893-902，Host 直折已覆盖）+ 裁决 note 3 臂（approve/askUserQuestion/planVerdict，~917-939，Request 路径已覆盖，GW1 双发幂等折叠）；sessionCreated/sessionDisposed/error 3 臂被 Host 转换器内部复用（规范化入口，非 wire 残余）。C4b 删臂 = 删路由+7 臂，零行为变化（各臂的 Host/Request 等价路径已逐一核对存在）。
- napi：适配面同步（vscode 徽章已列 follow-up）。

**顺序**：C4a-1 桌面权威迁移✓ + C4a-2 compat 验证✓（生产发送方清零，vscode 除外——见上）→ **C4b 定界收窄**：本期可删 = 无任何消费方的面（FocusThread 变体+no-op 臂、ThreadListItem.unread 列、error-stub call、K9 死 schema——逐项 grep 证据后单批删除，J1/C3 门禁看守）；**死亡清单大宗**（注册表 4 臂 models/commands/threadsUpdated/ready + 裁决 3 臂 approve/askUserQuestion/planVerdict + 控制 3 臂 sessionCreated/disposed/error + compat 三件）**gated 于 vscode extension host 的 host 帧迁移**：sessionManager 的 awaitSession 谓词与审批流消费 note 形状事件（`ev.method === 'sessionCreated'` 等 12 消费点），parseFromServer 不把 host 帧归一为 method 事件——删臂即 vscode 建会话挂起（硬破坏，非 badge 类 UI 退化），sidebar webview 不受影响（sidebarProvider 原样转发全部 FromServer 含 host，webui store 已 onHostEvent 化）→ C4b 大宗+vscode host 迁移列为 PR 合并前硬 follow-up → C4c §J.6「零残留」声明随大宗删除真实化。
**门禁证据**：C3 `wire_surface!` 宏单源（表/match/样本同收敛）；J1 host+call+journal 三面门禁；ts-rs exact-key 守卫+fixtures 导出；桌面棘轮针面（source_gates）。
