# manox ↔ manox-app 交互协议 v3 —— AHP（Agent Host Protocol, channel 化）

> **W0 规格冻结文档，normative**。本文冻结 v3 的目标终态、身份映射、不变式处置、x-manox 扩展声明面、依赖选型、删除清单与分期门禁；实施偏离一律记入 §H as-built，不回改上文冻结文本。
> 取代关系：`docs/dsh-v2-architecture.md`（协议 v2，`manox-protocol`）自此进入冻结（只收 blocker 修复），其全部协议面到 v3 的映射以本文为唯一事实源。
> 权威优先级（对 AHP 侧）：**`types/` 与 `ahp`/`ahp-types` crate 为准**，AHP 文档仅作语义参考（其示例仍写 0.3.0、`annotations` 行缺失、`disposeChat` 矛盾，差异记 as-built）。

## Context：现状事实（已核实，两仓当时 main）

- 两仓**已经是**「一进程内、typed 的状态同步协议」：`dspo/manox` 的 `manox-protocol` v2（`crates/manox-protocol`，约 4.4k LOC）+ `manox-session-core` 的 `AgentServer` 单例（`agent_server.rs` 5588 行）经 `transport::RpcConnection` 的 `in_process_pair`（async_channel，进程内对象）与 GPUI 前端通信；`cx web` 另起 loopback WS（`ws/{mod,listener,connection}.rs`，`/ws` 单路由、`?token=`、`~/.manox/gateway-ws.json`、`~/.manox/gateway.lock` 单例），**同一套帧直挂 socket 上**，无翻译层。
- 协议 v2 已自带的协议级机制（与 AHP 高度重叠）：`Initialize{protocol_epoch}` 握手、`FollowSession` 流（`Snapshot`→`Entry`→`Projections`）、seq 单点盖章（L4）、投影 `key→{value,asOfSeq}` higher-seq-wins（L6）、溢出即 `StreamEnd{Resync}` 的重同步（L5）、客户端缺口修复引擎 `journal_stream.rs`（497 行）、六车道信封（`FromClient`/`FromServer`）、31 个 `ClientNote` + 18 个 `ClientCall` + 6 个 `ServerCall`（waterfall 审批/询问/计划评审）+ `HostEvent` 总线 + `wire_surface!` 声明面（`surface.rs` 1032 行）。
- 客户端侧（`dspo/manox-app`）已自行实现 AHP 意义上的 state store：`client_store.rs`+`client_store_handle.rs`（1153+2143 行）持 journal 窗口 + 投影面 + echo 表 + 传输状态，`journal_fold.rs`/`journal_translate.rs`（437+888）折叠与翻译，`multiplexer.rs`（1970）做 MsgId/StreamId 路由；`source_gates.rs`（432）用 grep 计数冻结「绕过网关的直读」，`views/*` 仍 import runtime Rust 类型（`ThreadEvent`/`Message`/`HistoryEntry`/`PermissionMode`…）并持 `ThreadHandle`「渲染镜像」。
- AHP 侧（`~/projects/github/agent-host-protocol` @ `ce728562`，spec **0.9.0**）：JSON-RPC 2.0 + URI channel（`ahp-root://`、`ahp-session:/<uuid>`、`ahp-chat:/<cid>`、`ahp-terminal:/<id>`、`ahp-changeset:/<id>`、`ahp-automations://`、`ahp-session:/<uuid>/annotations`、`ahp-resource-watch:/<id>`、`ahp-otlp:`、`mcp://`）；"每帧 params 顶层带 `channel`" 使 `(method, params.channel)` 即可路由；宿主权威状态 + 客户端乐观写前（`dispatchAction{clientSeq}` → 宿主回 `action` 信封带 `origin{clientId,clientSeq}`/`rejectionReason`）；全局单调 `serverSeq` 与 `reconnect{lastSeenServerSeq}` 的「重放或快照」二选一；能力协商；`_meta` 与 **`x-` 前缀**为合法私有扩展位。
- **必须自建（AHP 无宿主 SDK）**：`ahp`/`ahp-types`/`ahp-ws` 三个 crate 全是客户端（`ahp-ws` 只会 dial），没有任何 server/listener/session-manager/dispatcher/replay 设施。宿主半边——JSON-RPC 路由、全局 `serverSeq` 序号、快照与重连、动作校验/接受表、副作用派发（动作 → 真正跑 agent）、`resource*` 文件面——**是我们的新增维护面**。参考实现只有 VS Code（TS，`src/vs/platform/agentHost/node/`）、`pi-ahp`（TS，把 pi 暴露成 AHP host；pi 正是 manox 内核的上游）与一个 157 行的 dotnet 一致性 fixture。

### 用户裁决（2026-09-23）

- **范围：全量替换**——manox-app 与 manox 之间的交互协议改为 AHP（channel 模型 + JSON-RPC + 双方同码 reducer）；不做「只加适配面」，也不做「只内部 channel 化」。
- **四个诉求全部是验收面**：外部客户端互操作（VS Code Agents window / AHPX）、两仓解耦（UI 不再吃 runtime Rust 类型）、远程与多客户端、少维护自有协议。

### 一句话方案

**journal 仍是唯一 durable 权威（L3/L4/L10 存续），AHP host 把 journal 的追加翻译成 AHP 动作并广播，AHP 通道状态是 journal 的确定 fold；app 变成用 `ahp` crate（client + reducers）的纯 AHP 客户端。** 旧协议面（`manox-protocol` 全套 + session-core 的流/投影/waterfall 面 + app 的 client_store/journal_fold 一套）在 W4 整体删除，不做兼容层。

### 技术支点（决定本方案可行的四点）

1. AHP 明示传输由双方带外选定，**in-process message channel 合法**（`transport.md`）。
2. 9 个纯 reducer 已随 `ahp` crate 发布（`apply_action_to_{root,session,chat,terminal,changeset,annotations,resource_watch,automation,automation_run}` + `ReduceOutcome`，`ahp/src/reducers.rs`），规范的意图就是**宿主与客户端跑同一份 reducer 代码** ⇒ 收敛性由构造保证。
3. `ahp-types` 0.9.0 已含全部 state/action/command/notification 类型，**唯一前端叙事（TypeScript `types/` 为源、六语言生成）**。
4. 重连允许以**快照代替重放** ⇒ 首版不必自建重放缓冲。

---

## §A 目标终态

```
┌ app 进程（单二进制）────────────────────────────────────────────┐
│ GPUI agent-ui：AhpStore（ahp::Client + reducers + x-manox 状态） │
│        ▲ typed in-proc Transport（ahp::Transport impl）          │
│ manox-ahp host（同进程单例，L11）                                 │
└──────────┬──────────────────────────────────────────────────────┘
           │ axum `/ahp`（WS text frame，JSON-RPC 2.0，`?token=`）
           ▼
   VS Code Agents window · AHPX · Agent Console · 远程客户端
manox-ahp host
   ├─ channel registry：root / session(=thread) / chat(=session) / terminal
   ├─ 全局 serverSeq 单点盖章 · subscribe 快照 · reconnect
   ├─ 动作校验表（只接受我们声明的 client-dispatchable 子集）
   ├─ 副作用派发（chat/turnStarted → 内核 turn；toolCallConfirmed → 审批 settle）
   ├─ x-manox 扩展面（plan / work / metrics / workspaces / commands / modelchat）
   └─ resource* 最小面（工作目录围栏内）
           ▲ journal 条目（38 变体）全射翻译
manox 内核 ThreadCore + Journal v4（磁盘 .jsonl，生态工具仍可直读）
```

一句话：**journal（磁盘 .jsonl）仍是唯一 durable 权威；`manox-ahp` host 把 journal 追加全射翻译成 AHP 动作并广播，AHP 通道状态是该 journal 的确定 fold；manox-app 退化为 `ahp` crate（client + reducers）的纯 AHP 客户端，另经同一 in-proc `Transport` 抽象走进程内腿。** L1–L12 不变式的逐条存续/改写见 §C（唯一一份，不在本节重复）。

---

## §B 身份与映射（核心决策，W0 定稿）

### B.1 通道 / URI

| AHP channel | 映射到 manox | 说明 |
|---|---|---|
| `ahp-root://` | 全局 | `agents[].models`（=`ListModels`/`HostEvent::Models`）、terminals 目录、host config |
| `ahp-session:/<uuid>` | **thread**（侧栏一行） | 承载 title / project / `workingDirectories` / status 位集 / `chats[]` 目录 / `defaultChat` = thread 的 active-session 指针；pin、分组、手工排序落 x-manox |
| `ahp-chat:/<cid>` | **session**（一份 `.jsonl` journal） | turns / activeTurn / responseParts / toolCalls / pending / draft；`createChat{source:{kind:'fork'|'sideChat'}}` ↔ 分支/fork/leaf 重定向 |
| `ahp-terminal:/<id>` | terminal | `data`/`input`/`resized`/`claimed`/`exited`；多客户端同权（AHP 原生，优于现状 follow-only） |
| `x-manox-plan:/<chat-id>` | plan 模式 + plan 文件 + 评审 | 缺口补齐 |
| `x-manox-work:/<session-id>` | goal / 后台任务 / browser suites / 子代理树（depth/branch/agent_label） | 缺口补齐 |
| `x-manox-metrics:/<chat-id>` | 原 Q 面（`GetConversationInfo`） | turns/messages/per-model usage/成本/context%/git stats，按 cursor 变更推送 |
| `x-manox-workspaces://` | `manox-workspace` 域（目录行 + 有序会话账 + 全局 archive 集） | 缺口补齐（原 `ClientCall::Workspace`） |
| `x-manox-commands://` | 命令/skill 目录（原 `ListCommands`/`HostEvent::Commands`） | stateless 快照 + 变更通知 |
| `x-manox-modelchat:/<req-id>` | `ModelChat` 侧流（VS Code LM provider 的裸模型补全） | stateless 通道 |

### B.2 帧与传输

| v2 | v3 |
|---|---|
| `FromClient`/`FromServer` 六车道 | JSON-RPC 2.0：request/response、宿主→客户端 `action` 通知、客户端→宿主 `dispatchAction` 通知、per-channel 协议通知、宿主→客户端 request（`resource*` 与 `x-manox/*`） |
| `MsgId` 关联 | JSON-RPC `id`；写路径改为 `clientSeq` 乐观对账 |
| `StreamOpen/StreamItem(Entry/Projections)/StreamEnd{Resync}` | `subscribe`/`unsubscribe` + `action` 信封（`serverSeq`）；溢出 = 断连 → `reconnect`（快照腿） |
| `Snapshot{records,has_more,projections}` | `subscribe` 结果 `{resource,state,fromSeq}` + `view.turns` + `fetchTurns{cursor}` 分页 |
| `Initialize{protocol_epoch = 7}` | `initialize{protocolVersions:["0.9.0"],clientId,clientInfo,initialSubscriptions,locale}` → `{protocolVersion,serverSeq,snapshots,_meta:{"x-manox":{…}}}` |
| `RpcError{code,message,data.code}`（稳定串码） | AHP 标准码（`-32001…-32011`）+ x-manox 码区间，声明表单一事实源（C2 纪律保留：生产零无码） |
| `ServerCall` waterfall（Approve/AskUserQuestion/PlanVerdict） | **状态化**：`chat/toolCallReady`(+`options[]`)/`chat/toolCallConfirmed`；elicitation `chat/inputRequested`/`inputAnswerChanged`/`inputCompleted`；plan 走 `x-manox-plan/*`。多 owner 汇聚策略留在宿主侧，线下只暴露单一裁决结果与 `_meta.x-manox.deliveryId` 对账面 |
| 进程内 `in_process_pair` | `ahp::Transport` 的进程内 impl（typed 帧，不序列化）；WS 由宿主侧 `WebSocketUpgrade` 提供 |

### B.3 命令 / 动作全量映射（实施对照表，W0 逐条定稿）

**`ClientCall`（18）**

| v2 | v3 |
|---|---|
| `Initialize` | `initialize` |
| `OpenSession` | `subscribe(ahp-session:/<thread>)` |
| `ListThreads` | `listSessions` + `root/sessionAdded\|Removed\|SummaryChanged` 增量维持 |
| `ListModels` | root 快照 `agents[].models`（`SessionModelInfo{id,name,maxContextWindow,supportsVision,configSchema}`）+ `root/agentsChanged` |
| `ListCommands` | `x-manox-commands://` 快照 |
| `TerminalAttach` / `TerminalSnapshot` | `subscribe(ahp-terminal:/<id>)` + `terminal/claimed` |
| `ModelChat` | `x-manox/modelChat` 命令 + 流式通知（stateless 通道） |
| `RegisterSessionTools` | AHP 客户端工具腿（`activeClients`/`session.server_tools` + 归属客户端的 `chat/toolCallComplete`）；形状不足时兜底 `x-manox/invokeTool` |
| `CreateSession{cwd,project,initial_model,approval_mode,reasoning_effort}` | `createSession{channel:ahp-session:/<客户端铸 uuid>,provider,workingDirectories,config}`；`lifecycle:'creating'`→`session/ready`；幂等键 = 客户端铸 URI |
| `Submit{session_id,text,images,origin_rpc}` | `dispatchAction{channel:ahp-chat:/<sid>,clientSeq,action:{type:'chat/turnStarted',turnId,message:{text,origin:{kind:'user'},attachments:[embeddedResource]}}}`（`origin_rpc` 角色由 `clientSeq` 取代） |
| `Steer` | `chat/pendingMessageSet{kind:'steering'}` |
| `PageHistory` | `fetchTurns{cursor}`（翻页语义）；journal 条目级翻页由 `x-manox/fetchEntries` 兜底 |
| `Workspace{call}` | `x-manox-workspaces://` 上的命令/动作 |
| `GetConversationInfo` | `x-manox-metrics:/<chat-id>` 快照 |
| `CancelDelivery` | `x-manox/cancelDelivery`（撤回未决宿主→客户端请求；状态化后该面收窄） |
| `ForkSession` | `createChat{source:{kind:'fork'|'sideChat',turnId,selection?}}` + `x-manox/branchFork`（leaf 重定向/`BranchSummary` 语义） |

**`ClientNote`（27 + 3 compat）**

`DetachSession`→`unsubscribe`；`DisposeSession`→`disposeChat`/`disposeSession`；`DropQueued`→`chat/pendingMessageRemoved{kind:'queued'}`；`CancelTurn`→`chat/turnCancelled`；`SetModel`→`session/configChanged`（canonical 串）；`SetReasoningEffort`/`SetApprovalMode`/`SetBrowserSuite`→`session/configChanged`（ConfigSchema 枚举；browser suite 仍由 `x-manox-work` 记状态）；`SetCwd`→`session/workingDirectorySet|Removed|Replaced`；`SetPlanMode`→`x-manox-plan/planModeChanged`；`PlanSeedExecution`→`x-manox-plan/execute`；`Compact`→`x-manox/compact`；`Goal`→`x-manox-work/goalChanged`；`StopBackgroundTask`→`x-manox-work/backgroundTaskStopped`；`ArchiveThread`→`session/isArchivedChanged`；`PinThread`→`x-manox/pinnedChanged`（AHP 无 pin 位）；`InsertThreadBefore`/`InsertGroupBefore`→`x-manox/orderChanged`；`TerminalInput`/`TerminalResize`→`terminal/input`/`terminal/resized`；`CancelModelChat`→`x-manox/modelChatCancel`；`Shutdown`→`x-manox/shutdown`（连接级）；`AppendUserMessage`→`chat/pendingMessageSet{kind:'queued'}`；`AppendUiNote`→`chat/responsePart{kind:'systemNotification'}`；compat `CreateSession`/`Submit`/`Steer` 随 v2 一并删除（无兼容期）。

**`ServerCall`（6）**

`Approve`→`chat/toolCallReady{pending-confirmation, options[], _meta:{x-manox:{authId,summary,deliveryId}}}` + `chat/toolCallConfirmed{approved,selectedOptionId?,reason?}`；`AskUserQuestion`→elicitation 三动作（多客户端草稿同步白拿）；`PlanVerdict`→`x-manox-plan/verdictRequested`+`/verdict`；`BrowserOp`/`ClipboardRead`/`OpenExternal`/`InvokeClientTool`→宿主→客户端 request `x-manox/browserOp|clipboardRead|openExternal|invokeTool`（按 `capabilities` 路由，fail-closed 不变）。

**`HostEvent`（8 变体 + 死亡清单）**

`Ready`→`initialize` 结果；`Models`→root `agentsChanged`；`Commands`→`x-manox-commands://`；`ThreadsUpdated`→`listSessions` + `root/sessionSummaryChanged`（删全量快照腿）；`SessionStatus`→由 session/chat 的 `status` 位集派生（删独立事件）；`SessionCreated/Disposed`→`session/chatAdded` + `root/sessionAdded` / `disposeSession` + `root/sessionRemoved`；`Error`→JSON-RPC error + `chat/error` 响应部件。

### B.4 AHP 原生吃掉的自研物（直接删）

epoch 握手与 `wire_surface!` 四张表、客户端缺口修复引擎 `journal_stream.rs`、投影的**线上面**（`projection_hub`/`projection_cache` 的推送职责）、`ServerNote` 双发窗口与死亡清单、过渡列表通道（`threadsUpdated`/`models`/`commands` 全量快照）、`answer_kind` 能力协商、`StreamEnd{Resync}` 语义（由 reconnect 承担）。

### B.5 AHP 无对应（必须扩展或降级）

plan 模式与 plan 制品/评审；goal；compaction（journal 重写，AHP 无）；子代理层级（AHP 是扁平的 worker chat，无 depth/父子树）；thread pin + 手工排序 + 分组；workspace 行域；后台任务；browser suites；命令/skill/插件目录；Q 面聚合；客户端工具（`RegisterSessionTools` 方向相反）。以上全部落 `x-manox/*`（合法 `x-` 前缀 + `_meta`），第三方 host 下优雅降级。

---

## §C 不变式（L1–L12 的存续与改写；本节为唯一一份）

| 旧 | v3 处置 |
|---|---|
| L1 句柄锁不可重入 | 原样 |
| L2 前端能力 `BoxFuture` | 原样 |
| L3 一切状态变更皆 journal 条目 | **原样**（AHP 动作面是 journal 的投影，不是新权威；「字段变了但无事件」仍结构不可能） |
| L4 seq 单点盖章 | 双层：journal 内 seq 仍单点盖章（磁盘/回放契约不变）；网关新增 **全局 `serverSeq` 在动作派发点单点盖章** |
| L5 快照不丢、溢出即重同步 | 改写为：snapshot 永不丢（subscribe 快照 / reconnect 快照）；高频 delta 用 `subscribe.delivery.maxLatencyMs` 合并；网络载体溢出 = 断连 → 客户端自动 `reconnect` 取快照；进程内载体仍无界 |
| L6 客户端零领域 fold | **强化**：AHP reducer 是客户端唯一 fold，视图不得解析 provider/工具词汇 |
| L7 写只回 receipt | AHP 无 receipt：写是 `dispatchAction` 通知，回执 = 动作回声 + `serverSeq` + `origin{clientId,clientSeq}` 对账（乐观写前，比 receipt 更严格） |
| L8 wire 身份 canonical | 原样：`AgentInfo.provider` + `SessionModelInfo.id` = canonical `provider/model` 串 |
| L9 五面表达 UI 值 | 改写为：**AHP 通道状态 + x-manox 扩展状态 + 宿主→客户端请求**；组件层仍禁私开同步通道 |
| L10 重放等于内存 | **原样**，并加一条门禁：`快照 == fold(重放)` |
| L11 单网关 | 原样（一进程一个 AHP host，单 `serverSeq` 域） |
| L12 声明面即公开契约 | 语义保留、载体更换：AHP 自身 registry + 我们**唯一一张** x-manox 扩展声明表 + journal 词汇表（磁盘面） |

---

## §D x-manox 扩展面（单一事实源，随 `initialize` 协商）

### D.1 声明块

```jsonc
"_meta": { "x-manox": { "version": 1,
  "channels": ["x-manox-plan:/", "x-manox-work:/", "x-manox-metrics:/",
               "x-manox-workspaces://", "x-manox-commands://", "x-manox-modelchat:/"],
  "clientDispatchableActions": ["x-manox-plan/planModeChanged", "x-manox/pinnedChanged", "..."],
  "commands": ["x-manox/compact", "x-manox/modelChat", "x-manox/fetchEntries", "..."],
  "serverRequests": ["x-manox/browserOp", "x-manox/clipboardRead", "x-manox/openExternal", "x-manox/invokeTool"],
  "capabilities": { "multipleChats": {"fork": true, "sideChat": true},
                    "multipleWorkingDirectories": { "immutablePrimary": false } } } }
```

### D.2 扩展通道与其承载的领域状态

| 扩展通道 | 承载的领域状态 | AHP 缺口来源 |
|---|---|---|
| `x-manox-plan:/<chat-id>` | plan 模式开关、plan 文件/制品内容、评审请求与裁决（`verdictRequested`/`verdict`/`execute`/`planModeChanged`） | AHP 无 plan 模式与 plan 制品/评审 |
| `x-manox-work:/<session-id>` | goal、后台任务、browser suites 运行态、子代理树（depth/branch/agent_label） | AHP 是扁平 worker chat，无 depth/父子树；goal/后台任务/browser suites 无对应 |
| `x-manox-metrics:/<chat-id>` | 原 Q 面聚合：turns/messages、per-model usage、成本、context%、git stats，按 cursor 变更推送 | AHP 无按需聚合（Q）面 |
| `x-manox-workspaces://` | `manox-workspace` 域：目录行 + 有序会话账 + 全局 archive 集（原 `ClientCall::Workspace`） | AHP 无 workspace 行域 |
| `x-manox-commands://` | 命令/skill/插件目录（原 `ListCommands`/`HostEvent::Commands`）：stateless 快照 + 变更通知 | AHP 无命令/skill 目录 |
| `x-manox-modelchat:/<req-id>` | `ModelChat` 侧流（VS Code LM provider 的裸模型补全）：stateless 通道 + `modelChatCancel` | AHP 无裸模型侧流 |

### D.3 命名与共存规则（normative）

- **单一事实源**：一切 `x-manox` 命名（通道、动作、命令、serverRequest、`_meta` 键、错误码区间）必须声明在本文这一张表（及其 D.1 声明块）里；代码中不得出现未声明的 `x-manox` 名。新增 = 先改本文，再改代码。
- **未知容忍**：对端收到未知扩展通道/动作/`_meta` 键时忽略不断连、不报错（与 §附.3 Assumption 4 一致）。
- **第三方 host 优雅降级**：manox-app 连非 manox host 时，`_meta["x-manox"]` 缺失即隐藏对应 UI 能力面，绝不因扩展缺失而报错或降格核心流程。
- `x-` 前缀与 `_meta` 是 AHP 0.9.0 明示的合法私有扩展位；`x-manox` 不承诺第三方 host 支持。

---

## §E 依赖与选型决策（不再留待实施者决定）

1. **依赖**：`ahp-types`（wire 类型）+ `ahp`（client + **reducers**）**精确 pin `=0.9.x`**；`ahp-ws` 进 manox-app 侧测试与外部客户端 e2e（宿主侧 WS **不用** `ahp-ws`，它只能 dial；宿主用 axum `WebSocketUpgrade` + 自写 `ahp::Transport` impl——架构参考，禁止复制其代码，遵项目「禁抄袭第三方 crate」纪律）。
2. **crate 布局**：新增 `crates/manox-ahp/`，承载 **AHP 协议的宿主半边 + journal→AHP 翻译 + x-manox 扩展声明面**，初始模块面为 `wire.rs`/`router.rs`/`sequencer.rs`/`channels/{root,session,chat,terminal}.rs`/`translate/`/`ext/`/`resource.rs`/`transport/{inproc,ws}.rs`/`error.rs` + `tests/`；**其 `Backend` trait 缝由 `manox-session-core` 实现**（宿主语义动作最终落到 session-core 的网关与内核调用）。`manox-protocol` 删除（见 §F），不保留 `manox-protocol` 空壳。
3. **单网关单端口**：同一 axum listener 只留 `/ahp`（v2 的 `/ws` 直接删除，激进纪律）；token 与 `gateway.lock` 单例纪律不变，端点文件更名 `~/.manox/ahp-ws.json`。
4. **协商与版本**：`protocolVersions:["0.9.0"]`；扩展经 `_meta["x-manox"]`（未知键方忽略不断连）；我们**只 advertise 并实现真正用到的动作子集**（约 35 个宿主发射动作 + 必须接受的 client-dispatchable 子集），未实现的动作一律拒（`rejectionReason` 或 x-manox 稳定码）。
5. **错误码**：AHP 标准码 + `x-manox/*` 码表（单一声明表；C2「生产零无码」结构门禁保留）。
6. **能力**：advertise `multipleChats{fork,sideChat}`（= 已有分支/fork）；`multipleWorkingDirectories`（= 已有 `workingDirectories` 围栏，先不开 `immutablePrimary`）。
7. **durability**：AHP 无持久化要求，**journal 仍是唯一 durable**；宿主启动时由 journal fold 得到通道状态，AHP 快照 = 该 fold 的输出。

---

## §F 删除清单（W4 收口；LOC 为当时实测）

### F.1 manox

- 删 `crates/manox-protocol/`：`msg.rs` 634 / `client.rs` 469 / `server.rs` 319 / `stream.rs` 479 / `surface.rs` 1032 / `transport.rs` 494 / `handshake.rs` 113 / `journal_stream.rs` 497 / `workspace.rs` 86 / `answer_kind.rs` 47 / `wire.rs` 95 + `tests/{surface_coverage,journal_proptest,journal_vectors}.rs`。
- 保留并搬迁：journal 词汇表 `journal.rs`(377) + `base64_bytes.rs`(53) → **新增叶子 crate `crates/manox-journal/`**（纯磁盘格式，与 AHP 无关，也不属于任何协议面）。之所以是独立叶子 crate 而非并入 `manox-session-core`：它要打断 `manox-ahp → manox-session-core → manox-ahp` 的成环——`Backend` 缝由 `manox-session-core` 实现（§E.2），而 journal→AHP 翻译住在 `manox-ahp`，两侧都只能依赖这个叶子。**迁移窗口内 `manox-protocol` 以 `pub use manox_journal as journal;` 重导出它**，使 v2 继续编译到 §G W4 为止。
- `crates/manox-ahp/` 承载：AHP 协议宿主半边 + journal→AHP 翻译 + x-manox 扩展面（§E.2）；`Backend` trait 缝由 `manox-session-core` 实现。
- `crates/manox-session-core/`：删 `follow.rs` 614、`projection_hub.rs` 249、`projection_cache.rs` 192、`waterfall.rs` 444、`agent_client.rs` 227、`ws/connection.rs` 的 v2 帧路径；`projections.rs` 854 与 `translate.rs` 952 重写为内部的 `journal → AHP 动作 / 通道状态` 折叠（不再有独立「投影线上面」）；`agent_server.rs` 5588 瘦身（六车道/RpcPeer/StreamId/owner 表 → AHP host 的通道注册与副作用派发，即 `Backend` 实现侧）；`journal_query.rs` 360 的 `PageHistory` 改喂 `fetchTurns`、`ConversationInfoCache` 改喂 `x-manox-metrics`；`workspace_serve.rs` 205 改喂 `x-manox-workspaces://`；`model_chat.rs` 578 改喂 `x-manox-modelchat`；`ws/{mod,listener}.rs` 保留网关骨架（token/Origin/单例），路由 `/ws` → `/ahp`；`~/.manox/gateway-ws.json` → `ahp-ws.json`；`cx web` → `cx ahp`。
- `crates/manox-napi`（休眠）：会话接线改为进程内 AHP 客户端（小改）。

### F.2 manox-app

- 删 `client_store.rs` 1153、`client_store_handle.rs` 2143、`journal_fold.rs` 437、`journal_translate.rs` 888、`server_note_translate.rs` 164；`multiplexer.rs` 1970 → 单 `AhpClient` + 多订阅（大幅缩水）；`source_gates.rs` 432 重写为「视图只许读 AhpStore」的 grep 门禁；`workspace.rs` 去 `ThreadHandle`「渲染镜像」与 `provider_glue` 直读（模型面改读 root `agents`）；`views/message.rs` 的工具/审批卡改由 AHP tool-call 状态机渲染；`UI-MAP.md` 同 PR 更新。

---

## §G 分期与门禁

### W0 规格冻结（manox）

- 交付：`docs/ahp-v3-architecture.md`（本文：映射表终稿 + x-manox 声明面 + 删除清单 + as-built 位）；`docs/dsh-v2-architecture.md` 顶部加「v2 已由 v3 取代，映射见 ahp-v3-architecture.md」指针（不改写历史章节）；本地拉起 VS Code Agents window 与 AHPX 作为外部验收客户端（记录 `settings.json` 与连接方式）。
- 门禁：映射表与扩展面经用户确认；v2 进入冻结（只收 blocker 修复）。

### W1 宿主骨架（manox，纯新增）

- `crates/manox-ahp/`：JSON-RPC 编解码（`ahp-types::messages`）、`(method, channel)` 路由、全局 `serverSeq` 单点盖章、通道注册与订阅管理、`initialize`/`subscribe`/`unsubscribe`/`ping`/`reconnect`（**快照腿**）/`listSessions`/`createSession`/`disposeSession`/`createChat`/`disposeChat`、进程内 `Transport` impl、`/ahp` WS 路由、`cx ahp` headless 宿主。
- 门禁：dev-dep `ahp` + `ahp-ws` 客户端经**进程内与 WS 两路**跑通 root/session/chat 的 init/subscribe/reconnect/unsubscribe；`cargo clippy --all-targets -- -D warnings`；`script/gates.sh`。
- 中间绿：v2 面零改动（新增，不修改）。

### W2 域翻译与状态收敛（manox）

- `translate/`：journal 条目 **38/38 全射** → AHP 动作（消息、流式 delta 的 create-then-append、tool-call 生命周期、usage、lifecycle、title/activity、workingDirectories、config 承载 model/effort/approval/browserSuite、compaction、分支/摘要）；projection 值 → 通道状态字段。
- 审批面：`Approve` → `toolCallReady`(+`options[]`)/`toolCallConfirmed`（含 mid-execution 再确认、可编辑参数、result confirmation 三态的映射取舍）；`AskUserQuestion` → elicitation；`PlanVerdict` → x-manox-plan。
- 副作用派发：`chat/turnStarted|pendingMessageSet|turnCancelled|toolCallConfirmed|inputCompleted|isArchivedChanged|workingDirectory*` 等 → 现有内核调用（原 27 个 note 的落点）。
- x-manox 全线 + `resource*` 最小面（`resourceRead/Write/List/Delete` + ContentRef，落在会话工作目录围栏内，fail-closed）。
- 门禁：①**收敛性证明**——对任一 journal，`宿主状态 == reduce(快照, 已发信封序列)`（用 `ahp` crate 的 reducers 跑同一批信封）；②全射/覆盖门（38 条目 ⇒ 动作 ⇒ 声明表行，编译期 + 测试期联动，沿用 v2 文档 §J.4 口径）；③`快照 == fold(重放)`（L10 新面）；④真客户端 smoke，**实施期拆为两条**：**④a** 官方 TS client over WS（`script/ahp-client-smoke/smoke.mjs`，已绿，见 §H.2e）；**④b** 真 VS Code Agents window 发 prompt、看到工具卡与审批、看 plan，AHPX 列会话并收流（需先在 hcode fork 里加一条可插拔的 connection 来源，独立立项）；⑤错误码纪律。
- 中间绿：v2 继续服务桌面；AHP 面可独立跑。

### W3 客户端切换（manox-app）

- 新增 `ahp_store.rs`（`ahp::Client` + reducers + x-manox 通道状态）、`transport_inproc.rs`（typed `ahp::Transport`）；`ConversationState` 由 `ChatState.responseParts` 派生；`multiplexer.rs` 收敛；删 `client_store*`/`journal_fold`/`journal_translate`/`server_note_translate`；`source_gates.rs` 重写；`UI-MAP.md` 更新。
- 门禁：manox-app 全量门禁（clippy `-D warnings`、`cargo test`、`fmt`）+ **逐能力手工 e2e 清单**（submit/steer/queued/cancel/审批/询问/plan/分支与恢复/终端/模型与 effort 切换/后台任务/工作区排序/多客户端同权/长会话翻页）；`script/local-manox.sh on` 联调。
- 中间绿：切换后 app 与 v2 面零耦合。

### W4 v2 拆除（manox）

- **实施期修订（crate 形状）**：计划把 `adapter/` 放在 `crates/manox-ahp` 内，实施拆成了
  `manox-ahp`（transport-agnostic host + translate，不依赖 `manox-agent`，可用假 backend 单测）
  + `manox-session-core/src/ahp/`（`Backend` 实现，认识运行时）。这个解耦更好——`manox-ahp` 因此
  不依赖 `manox-agent`，`Backend` trait + `DispatchOutcome` 三值就是它的收益，单向依赖规则真的成立。
  **但它改变了 W4 的形状**：`manox-session-core` **不能**再按 §F 整体删除，因为
  `src/ahp/`（约 2.8k 行）住在里面。W4 需要给 `manox-session-core/src/ahp/` 一个新家——
  候选是新 crate `manox-ahp-runtime`，或搬进 `manox-agent`。**倾向前者**（保持
  `manox-agent` 不认识 AHP）。
- **实施期修订（新增 crate）**：`crates/manox-journal`（`journal.rs` + `base64_bytes.rs`）是计划里
  没有的落点——把**磁盘**词汇从 `manox-protocol` 里救出来成为叶子 crate，避免打断依赖拓扑
  （`manox-ahp` 需要词汇做翻译，`manox-session-core` 需要 `manox-ahp` 做宿主）。§F 删除清单需同步。
- **W4 前置核查（已做，2026-09-23）**：`manox-journal` 里有没有残留 v2 词汇——否则删
  `manox-protocol` 会把 v2 的一部分以新名字留下来。**结论：代码面零残留**。该 crate
  `use` 只有 `serde`（无 `FromClient|FromServer|ClientCall|ClientNote|ServerCall|ServerNote|
  PROTOCOL_EPOCH|StreamFrame`），453 行里没有任何帧类型或网关词汇；它承载的是 `.jsonl` 的行形状，
  而 v3 切换**不改磁盘格式**（这正是 R1 里「锁旧 rev 的 client 仍能工作」的前提）。
  **但文档面有漂移且已修**：`journal.rs` 的头注释自称 "Journal wire vocabulary" 并把
  `StreamFrame::Entry`/`Snapshot`（v2 帧类型）写成自己的声明面——那是**传输**的关切，不是这个
  crate 的。已改写为「disk vocabulary」，并删掉三处会随 W4 变成悬空引用的 `StreamFrame::*`。
  唯一保留的 v2 味道是 `running` / `pending_auth` 两个词——实测它们**两边都在用**
  （`manox-ahp/translate/actions.rs:146` 也读 `"running"`），是共享词汇而非残留。
- 按 §F 删除清单执行；`manox-napi` 改 AHP；文档补 §H as-built 章。
- 门禁：全仓 `script/gates.sh`；**grep 门禁**（生产区零 `FromClient|FromServer|ClientCall|ClientNote|ServerCall|ServerNote|PROTOCOL_EPOCH|StreamFrame`）；双路径一致性（in-proc typed ≡ WS serde）；`journal_replay_is_consistent_across_disk_reload` 仍绿。
- 中间绿：删除面已零消费者。

### W5 硬化与生态

- 有界重放缓冲（按 `serverSeq` 重放替换快照腿）、`delivery.maxLatencyMs` 合并调优、`resource*` 完整面（大输出 ContentRef、图片引用、客户端 `virtual://` URI、工作目录围栏）、远程与鉴权（token 已有；AHP `protectedResources`/`authenticate` 接入 provider 凭据后续议）、多客户端并发与 owner 语义、长会话快照成本度量。
- 门禁：`ahp-*` minor 升级演练 checklist（升级即以收敛性门禁为护栏）；上游 `types/test-cases/reducers/*.json` 与我们信封的一致性抽样。

### 跨仓协同与 PR 顺序（每步 main 常绿）

1. **PR-A（manox）**：W0+W1+W2（纯新增 `manox-ahp` + 文档；v2 不动）。分支 `codex/ahp-host`，worktree 实施。
2. **PR-B（manox-app）**：W3（切到 AHP；`Cargo.lock` bump 到 PR-A 的 rev）。分支 `codex/ahp-client`；`script/local-manox.sh on` 联调，off 形态验证。
3. **PR-C（manox）**：W4（删 v2；`manox-protocol` 整体消失）。分支 `codex/ahp-remove-v2`，基于 PR-A 之后 main。
4. **PR-D/E**：W5 各自仓，单独 PR（不并入 A/B/C）。

- **Cargo.lock rev 锁定即顺序容错**：manox-app 以 git 依赖 + `Cargo.lock` 锁定 manox 的 rev，PR-A/PR-C 的 main 移动都不会波及尚未切换的 app 锁定版本 ⇒ 不存在「双协议并存」窗口，也不需要并存。
- 依赖 bump 纪律照 manox-app AGENTS.md：合并前 `cargo update -p manox-agent` 抬升同一 git source 的全部依赖。
- 门禁权威：manox 以 `script/gates.sh` 完整四腿为准；app 以 clippy `-D warnings` + 全量 `cargo test` + `fmt` 为准。
- **合并必须等用户明确指示（含 squash）**；提交信息不带 Co-Authored-By。

---

## §附 显式不做 / 风险 / Assumptions / 验收标准（非架构轴，随规格一并冻结）

### 附.1 显式不做（本期范围外）

- AHP 的 changesets / annotations / comments（上游仍是空文件）/ automations / OTLP telemetry / `mcp://` channel / customizations(Open Plugins) / `MultiHostClient` / `protectedResources`+OAuth：不实现、不 advertise。changeset 的 diff-review 能力列入 W5 之后的候选（配合 git 面板再议）。
- 除 VS Code / AHPX 之外客户端兼容性承诺；x-manox 扩展不承诺第三方 host 支持。
- 多窗口（保持当前单窗口约束）。
- **不做 v2→v3 兼容层、不写 `legacy_*`/双协议窗口、不做 fallback 兼容读**（激进开发纪律；剪切靠 rev 锁定完成）。

### 附.2 风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| AHP 0.9.0 pre-1.0，每个 minor 已历史性引入破坏 | 适配面反复返工 | 精确 pin `=0.9.x` + 升级任务化 + 收敛性门禁当护栏 + 扩展面自有声明表隔离；把「升级」写成独立清单 |
| 无宿主 SDK，自建面大 | 工期与维护面上升（短期） | W1/W2 即交付主体；只实现用到的动作子集；重放腿延后到 W5（spec 允许快照代替）；长期净删 v2 全家 |
| AHP 文档漂移（示例仍写 0.3.0、`annotations` 行缺失、`disposeChat` 矛盾） | 实现依据错位 | **以 `types/` 与 `ahp`/`ahp-types` crate 为准**，文档仅作语义参考；差异记 as-built |
| 特性缺口（plan/compaction/子代理层级/pin+排序/workspace/命令目录/Q 面） | UI 功能退化 | 全部落 x-manox 扩展并在 W0 定稿；第三方 host 下优雅降级（`_meta` 缺失即隐藏） |
| 迁移期两仓不同步 | 断裂/带病 main | 三 PR 顺序 + rev 锁定 + `local-manox.sh` 联调 + 逐能力 e2e 清单 |
| 背压语义变化（原 Resync 帧 → 断连重连） | 长流体验 | 进程内无界（沿 v2 C6 决断）；WS 用 `delivery.maxLatencyMs` 合并 + 断连自动 reconnect；W5 补 replay buffer |
| `resource*` 面越权读写 | 安全 | 只暴露会话工作目录子树 + granted-root 围栏；loopback+token 不变；fail-closed |
| 快照体积（长会话全量 state） | 卡顿 | `view.turns` 尾部裁量 + `fetchTurns` 分页 + `x-manox-metrics` 让 Q 面不占推送；W5 度量 |
| 生产侧依赖客户端 crate（reducers 在 `ahp` 内） | 概念错位/依赖偏重 | 生产侧只经 `ahp-types` + `ahp::reducers`；记录为 Assumption，上游若加 feature 开关则切换 |
| 性能/渲染回归（视图改由 AHP 状态派生） | 体验退化 | W3 以「逐能力 e2e 清单 + 视觉零回归」为验收；`agent-ui` 以 selector-only 约束重构 |

### 附.3 Assumptions

1. `ahp`/`ahp-types` 0.9.x 的 reducers 可作宿主侧权威 fold（规范明示双端同码），我们**只依赖不复制**。
2. 进程内 channel 作为 AHP 传输合法（`transport.md` 明示 in-process message channel 之一），且 typed 帧（不序列化）满足一致性要求（双路径一致性测试守护）。
3. 我们只 advertise 并实现自己真正用到的动作子集；未实现动作拒绝（不是静默 no-op）。
4. AHP 客户端对未知 `_meta` 键与未知 `x-` 方法必须忽略而不致断（VS Code 客户端实测确认）。
5. `gateway-ws.json`→`ahp-ws.json`、`cx web`→`cx ahp`、`/ws`→`/ahp` 无外部消费者（webui 前端已随仓库边界裁决删除）。
6. i18n 与 UI 文案完全留在 manox-app，协议改造不动语言轴。

### 附.4 验收标准（用户视角）

1. VS Code Agents window 与本机 manox host（`cx ahp`）连接后：列会话、发 prompt、看流式回复与工具卡、做一次审批、看一次 plan 评审、切模型，全流程可用。
2. AHPX CLI 能 `listSessions` 并收发一轮。
3. manox-app 桌面功能与改造前逐条对齐（W3 的 e2e 清单全绿），且 `agent-ui` 不再 import 任何 manox runtime 类型（除 `manox-ahp` 的 AHP 类型）。
4. `manox-protocol` 从 manox 仓消失；两仓生产区搜不到 v2 帧词汇；`script/gates.sh` 与 app 全量门禁绿。

### 附.5 实施方式

按 **`/gitwork:deliver`** 实施：两仓各自 worktree（`/private/tmp/<repo>--<branch>`）+ `codex/` 分支 + 正交 PR（PR-A → PR-B → PR-C）；每 PR 写清 Test Plan 与 Assumptions；门禁以 `script/gates.sh`（manox）/ clippy+test+fmt（manox-app）为唯一权威，不做手挑测试的假绿声明；已知沙箱环境性失败与整机并发 flake 记录在案、不计回归；**PR 创建后停手，合并等用户明确指示**。

---

## §H as-built

<!-- 本节随各期落地逐段追加，记录与上文冻结规格的偏离（含 AHP 上游文档漂移的具体差异、
     映射表的实施期修订、W1 实际模块面与 §E.2/§F 的出入）。规格正文不回改，偏离只进本节。 -->

### H.1 W0/W1/W2（宿主侧 + 运行时只读面）已落地，2026-09-23

分支 `codex/ahp-host`（5 个提交，未 push、未开 PR）。已落地面与门禁：

- **W0 规格冻结**：本文档 + `docs/dsh-v2-architecture.md` 顶部取代指针。
- **journal 词汇叶子化**：新增 `crates/manox-journal/`（`journal.rs` 377 + `base64_bytes.rs` 53），
  偏离 §F/§E 的「迁入 manox-session-core」——叶子 crate 才不打断依赖拓扑
  （`manox-ahp` 需要词汇做翻译，`manox-session-core` 需要 `manox-ahp` 做宿主）。
  迁移窗口内 `manox-protocol` 以 `pub use manox_journal as journal;` 重导出，v2 面零改动。
- **W1 宿主骨架**：`crates/manox-ahp/`（wire/router/sequencer/channels/backend/ext/error/
  translate/transport{inproc,axum_ws}）。与 §E.2 的实施期修订：`Backend` 缝的实现方是
  `manox-session-core`（见下），`Host` 自带 `chat_state`/`session_state` 读面供收敛性门禁使用。
- **W2 宿主侧翻译**：`translate/actions.rs` 的状态机（`Translator::on_entry`）覆盖全部
  journal 变体；`translate/mod.rs` 的 `target_of` 穷举即「新内核变体不落 wire 就编译失败」的门。
  实施期修订：扩展动作的 `x-manox/*` 清单在 W2 增补了 `x-manox-work/activeToolsChanged`、
  `x-manox/labelChanged`、`x-manox/sessionInfoChanged`、`x-manox/leafChanged`（AHP 无对应位）。
- **W2 运行时只读面**：`crates/manox-session-core/src/ahp/` 的 `chat_state`/`session_state`
  折叠（journal → AHP 通道状态），落定「宿主/客户端/冷启动折同一函数」的 L10 承诺；
  诚实留空项见该模块文档头（live 连接事实、pin/label、会话级 token 汇总）。
- **门禁**：`script/gates.sh` 六腿全 PASS（fmt/prod-libs/lean-libs/clippy/test-real/test-clean）；
  `cargo test -p manox-ahp --features axum-ws` = 26 绿（14 单测 + 9 进程内 + 2 收敛性 + 1 WS）；
  `cargo test -p manox-session-core` = 172 绿。

### H.2 收敛性门禁抓到并修掉的缺陷（留档）

首次运行 `crates/manox-ahp/tests/translation_convergence.rs` 即失败：翻译把
`Approval{kind:"request"}` 也走了一遍 `chat/toolCallConfirmed`（此时 `verdict` 为空 ⇒ 读作拒绝），
于是**正在等用户裁决的 tool call 被提前取消并 settle**，其后的 `ToolResult` 再也无法 complete，
终态为 `Cancelled(Skipped)`。修法：request 只发 `chat/toolCallReady` + `session/inputNeededSet`；
confirmed 与 `session/inputNeededRemoved` 只属于 decision 腿。修后轨迹
`Run → Ready → Confirmed → Complete(Completed{success:true})`，收敛与结构断言双绿。

### H.2b 两条传输腿落地（本轮新增，2026-09-23）

- **宿主可服务真实运行时**：`crates/manox-session-core/src/ahp/backend.rs` 实现 `manox_ahp::Backend`——
  播种=journal 折叠（与客户端同码），并为每个会话起 bridge：**先订阅 kernel journal feed、再折叠播种**，
  只转发 `seq > 播种 tail` 的事件（lagged 则重折重播），因此「快照 + 增量」严格不相交；
  写入映射到既有 intent（`chat/turnStarted` → `AgentServerInner::submit`，`createSession` →
  `create_session_request`，`disposeSession` → `dispose_session`），未接线的一律**响亮拒绝**。
- **两条传输腿同一宿主（L11）**：`ahp/runtime.rs` 提供进程单例；`inproc()` 是 **over channel**（进程内 typed 帧，
  桌面腿），`router("/ahp")` 是 **over websocket**（网关腿）。`ws/listener.rs` 在同一 listener、同一 per-boot token、
  同一机器锁下把 `/ahp` 与 v2 的 `/ws` 并挂（v2 路由留到 W4 摘除）。根通道按 L8 列活 provider 目录（canonical
  `provider/model`）。
- **门禁证据**：`cargo test -p manox-session-core` = **173 绿**，含新增的**传输平价测试**
  （`ahp::runtime::tests::in_process_and_websocket_legs_share_one_host`：一次 publish 同时到达进程内与
  WebSocket 订阅者，且 `serverSeq` 相同）；`clippy -D warnings` 与 `fmt --check` 干净；
  全 workspace 测试在**真实 HOME 与 hermetic clean HOME** 两种形态下均绿（clean 腿本轮因 /private/tmp 被系统清空、
  临时 HOME 需重新物化工具链而在 rustup shim 处挂起，故改为「预置工具链的 clean HOME」等价跑法：隔离属性不变）。
- **仍未落地**：终端通道、`createChat`/`disposeChat`（分支/fork）、`fetchTurns` 分页、x-manox 命令面、
  `resource*` 文件面、`Backend::dispatch` 的其余动作映射（当前除 turn 起点外全部 `Rejected`）。

### H.2c W2 写面与文件面落地（本轮新增，2026-09-23）

`Backend::dispatch` 从此不再只有一个 turn 起点：accepted action 落到**既有**运行时 intent
（不是第二份实现），未接线的仍响亮 `Rejected`。

- **dispatch 映射**（`ahp/backend.rs`）：`chat/turnStarted`→`submit`；`chat/pendingMessageSet`→`steer`；
  `chat/pendingMessageRemoved`→`drop_queued`；`chat/turnCancelled`→线程 `cancel`；
  `chat/toolCallConfirmed`→`respond_authorization`；`chat/inputCompleted`→`respond_question`；
  `session/configChanged`→`set_model`/`set_reasoning_effort`/`set_approval_mode` 扇出；
  `session/workingDirectorySet`→`set_cwd`；`session/isArchivedChanged`→`archive_thread`。
  实施期修订：`SessionIsReadChanged`/`ChatDraftChanged`/`ChatTurnResume`/
  `ChatToolCallResultConfirmed`/`ChatInputAnswerChanged`/`ChatQueuedMessagesReordered`/
  `SessionActiveClientRemoved` 归 **`Ignored`**（reducer 已折叠、运行时无对应 intent），
  回显而非拒绝——拒绝会告诉订阅者「你看得见的状态不是真的」。
  **`SessionTitleChanged` 不在此列**：重命名实现为真动作（见 §H.2d）。
- **审批身份单源**：settle key 走 action 的 `_meta["x-manox"]["authId"]`——即 translator 生成
  `toolCallReady` 时盖的同一个 key。**没有 authId 的确认不 settle**（`Ignored`）：猜一个身份会去
  回答**另一个** pending call。
- **`createChat`/`disposeChat` 落地**：`createChat{source:fork}` 映射到 `fork_session`，并把客户端
  选的 chat id 作为 fork 目标 id。为此 `ForkIntent` 增 `target_session_id: Option<String>`
  （`None` 仍为 v2 的随机铸造），并新增**已存在即拒绝**的守卫——`JsonlSessionStorage::create` 会截断，
  客户端选重 id 若不拦就是**毁掉一个已有会话**。`source:sideChat` 响亮 `Unimplemented`：
  「源不进可见历史」是 journal 没有行的上下文策略，伪造成 fork 会展示它不该展示的 turns。
  `disposeChat` 一并从 `CommandIntent::Declined` 转 `Implemented`（`command.rs` 的
  `declined_commands_are_known_but_not_served` 改为 `every_declared_command_is_served`）。
- **`resource*` 最小文件面**（`ahp/resources.rs`）：围栏即内核工具文件效果的同一围栏——路径**先
  canonicalize 再判**，判据是组件级 `starts_with`（故 `/work/grants-evil` 不匹配 `/work/grants`），
  `..`/符号链接/尚不存在的叶子都逃不出去（不存在的叶子经最近存在祖先解析后再判）。
  读允许落在 `~/.manox`（宿主自己的 journal/plan 制品是客户端正当渲染的内容）；**写只许授权根**
  ——状态根是宿主的记账，不是客户端可写面。denied 一律 `-32082`（`xManox/resourceDenied`）。
- **`x-manox/*` 命令面**：`x-manox/compact`→`compact`、`x-manox/planExecute`→`plan_seed` 落到既有
  intent；其余已声明命令答 `-32080`（`xManox/unsupported`）——「我声明了但不做」与「你请求写错了」
  必须可区分。
- **门禁证据**：`script/gates.sh` 六腿全 PASS（fmt/prod-libs/lean-libs/clippy/test-real/test-clean）；
  `cargo test -p manox-session-core` = **200 绿**；`cargo test -p manox-ahp --features axum-ws` = 39 绿；
  `cargo test -p manox-agent` = 533 绿。
  新测试全部用**类型化 action**（不经 JSON 往返）：`StateAction` 末尾的 untagged `Unknown(Value)`
  会让一个写错的字面量「成功」反序列化成无人匹配的 action，测试就会去断言兜底臂而非映射。
- **网关 `/ahp` 路由补测**：此前 `ws_conformance` 自绑 listener，绕过了网关自己的 token/origin 闸，
  于是「唯一没覆盖的路径恰好是安全边界」（token 是本地页面与宿主之间唯一的墙）。新测试
  `ws::tests::ahp_route_rides_the_gateway_token_gate` 走真实网关腿：无 token → 401；
  带 token 但 `Origin: http://evil.example` → 403；带 token + 非浏览器 origin
  （`vscode-file://vscode-app`，参考客户端的真实形状）→ 升级成功**并跑通 `initialize`**
  （证明路由真的挂了宿主，而不只是「一个会拒绝一切的 404 handler 也回 401」）。

### H.2d 重命名实现为真动作（本轮新增，2026-09-23）

`session/titleChanged` 曾被归入 `Ignored`（「运行时没有重命名 intent」）。**该前提只对了一半**，
复核后修正：持久化槽位、读取优先级、回归测试三样早就都在，缺的只是写它的那一跳。

- `ThreadSummary::title_override` 是 DB 列 + 结构体字段，`upsert` 语句里有它（`db/threads.rs:24,79,140,252`）。
- `display_title()` 的优先级是 **user rename > LLM title > summary**（`db/threads.rs:64-69`）——
  这个槽位就是为用户重命名建的。
- `db/mod.rs:396-400` 早有一个通过的 round-trip 测试，写 `Some("renamed")` 再断言读回。
- v2 没暴露重命名**不是**「运行时没有这个能力」，是 v2 没接。

实施按 `archive_thread` 的同一形状（`条目即权威`，K2/L3），**三条腿，缺一不可**：

1. **journal**：`thread_store.journal_title` 经 `dispatch_store_journal_row` 追加 `title` 行。
   kind 与 payload 与 `engine.rs:513` 为 `TitleChanged` 产的完全一致，所以用户重命名与模型生成标题
   落**同一种行**。这一行是 v2 与 AHP **两边唯一的路线**：v2 `projections.rs:203` 折
   `SessionTreeEntry::Title`，AHP `translate/actions.rs:595` 读 journal；而实时
   `ThreadEvent::TitleChanged` 恰在 `translate.rs:104` 的 `Skip` 表里——它本就不该被折叠，只该被 journal。
2. **内存行**：`rename_thread` 更新 `title_override` 槽（`display_title()` 第一优先级），
   facade 另经 `handle_notice` 拿到实时事件，侧栏立刻改名。
3. **sidecar**：`write_meta` 让名字免重启、免重扫存活。

**实施期纠错（重要，留档）**：本节初版把第 1 条写成「复用 `ThreadEvent::TitleChanged` 经
`handle_notice` 发出，因此与生成标题走同一条路」——**那句话是假的**。`handle_notice` 是
notice 链的**消费端**，在 engine 的 journal tap（`engine.rs:904-921`：发射点 → `notice_tx` →
tap 写 journal → `tap_tx` → `notice_rx` → facade）**之后**，所以注入进去的事件只到 facade、
**不产生 journal 行**。后果是真空洞：v2 客户端在重新 seed 前看不见重命名（`projections.rs:203`
折的就是那行），而 W1–W3 期间两套协议并存——正是要避免的跨协议静默分歧。

修法用既有先例 `journal_pinned_archived`（`thread_store.rs:1095`，store 级决定显式 journal）。
**注意那个模板恰好包含我漏掉的那一步**——`archive_thread` 有显式的
`journal_pinned_archived` 调用，正因为它是 store 级决定、也在 tap 之外。

**通用陷阱**（已同时写进 `engine.rs:908` 与 `thread_store.rs` 的注释）：tap 的
「no future emission site can forget to persist」保证**只对发射点成立**，对**注入点不成立**。
任何未来从 store 侧注入 `ThreadEvent` 的代码都会踩同一个坑。

**回归测试**：`rename_thread_appends_the_durable_title_row`（store 层）与
`title_changed_appends_the_journal_row`（AHP 层）都断言 **journal 行**而非只断言 summary。
两者都验证过「去掉 journal 那条腿即红」——在**全量 suite** 下复验过，只断言 summary 的
`title_changed_renames_the_session` 在同样条件下**照样绿**，这正是这个洞此前没被发现的原因。

**测试期第二个坑（留档）**：AHP 层那条测试初版复用共享 fixture 的 `s-dispatch`，
于是**单独跑绿、全量跑红**——因为 `engine::engine_routes` 是**进程级、按 thread id 索引**的 map，
先前某个测试的 `attach_engine` 为同一 id 注册了 route，本次 append 就被投递给那个已死的 actor，
既不落盘也不走 cold-append 回退（且不报错）。修法是给该测试一个 uuid 后缀的 session id。
教训：凡是直接断言 journal 落盘的测试，**session id 必须唯一**，否则会被引擎路由表串台。

4. **dispatch**：`Accepted`（非 `Ignored`），空白标题 `Rejected`。

为什么这不是「两种都不选」而是第三种：`Ignored` 在撒谎（reducer 折了、运行时没做），`Rejected`
让标准客户端一个正常操作报错。让重命名**真的生效**既不撒谎也不报错，还顺手激活了一个一直没接线的
既有能力。

### H.2e W2 门禁 ④a：官方 TS client over WS 已绿（本轮新增，2026-09-23）

计划把「真客户端 smoke」当一条门禁。它应当拆成两条——④a 现在就能做，④b 需要单独立项：

- **④a（已执行）**：用上游**官方 TypeScript client**（`@microsoft/agent-host-protocol@0.9.0`，
  与本仓 pin 的 `ahp-types = "=0.9.0"` 同版本）经 WS 打真实网关。
  工具：`script/ahp-client-smoke/smoke.mjs`（13 项断言全 PASS）。
- **④b（未做，独立立项）**：真 VS Code Agents window 的方言税（R5 那五处）。hcode 的连接来源
  只有 ambient/ssh/wsl 三条，全是起 VS Code 自己的 agent host，端点不可插拔；指向 manox 的 `/ahp`
  需要在 fork 里加一条 connection 来源。有价值，但不该继续挡着 W2 收口。

**为什么 ④a 是独立一层**：Rust 套件（`ws_conformance.rs`、进程内 e2e）用的都是 `ahp::Client`，
与宿主共享同一份 Rust `serde` 形状假设——我们把某个字段拼得与规范不同，在自己两半之间照样
round-trip、照样绿。TS client 由规范的 TS 源码生成，正是对这一类分歧敏感。

**执行记录**（`cargo run -p manox-session-core --example ahp_serve -- --port 0` 起网关，
读 `~/.manox/gateway-ws.json` 取 port+token，再跑 smoke）：13 项断言全 PASS——initialize 版本协商、
root 快照、`_meta["x-manox"]` 三查（version / channels 前缀）、ping、listSessions、
resolveSessionConfig 的 schema+values 对、completions 的 items、subscribe 快照、
reconnect 快照腿、dispatch 句柄。

**smoke 过程中实测到的两件事**（都不是宿主缺陷，但都值得留档）：

1. **`AhpClient` 的读泵不在构造函数里启动**——必须显式调 `client.connect()`。不调的话 socket
   收得到帧、但没有任何东西分发它们，于是每一个请求都在一个完全健康的宿主上超时。这条已写进
   smoke 脚本的注释，免得下一个人再花一小时。
2. **`completions` 的 params 形状**：规范要求 `kind`（`"userMessage"`）+ `text` + `offset`（数字）。
   宿主拒收缺 `kind` 的请求（`-32602`）是**正确**的——错的是我第一版 smoke 按
   `{text, position:{line,character}}` 发。留档以免下次误判为宿主 bug。

另：④a 首次执行时打的其实是**一个陈旧的 `ahp_serve` 进程**（当日 11:29 起的构建，早于
`resolveSessionConfig` 落地），于是 `resolveSessionConfig` 回 `-32601`。那不是回归，是二进制过期；
杀掉旧进程、用当前构建重跑即全绿。记在这里以免下次看到 `-32601` 又误判。

### H.2g 三条声明面缺陷（本轮修复，2026-09-23）

评审指出三条，逐条核实后全部成立，均已修：

**1. 穷尽性是假的（唯一的结构性倒退）。** `command.rs` 的 `intent()` 确实没有 `_` 臂，但它穷尽的是
**我们自己挑的 18 个名字**，不是上游 `CommandMap` 的 30 个——上游加命令不会编译失败。而且
`CommandIntent::Declined` 是死代码：没有一项是 Declined，于是 `of_method` 对表外名字回 `None`，
那个变体永不触发。**对自己子集的穷尽是同义反复。**
修法：表扩到**上游全部 30 条**（顺序即 `types/common/messages.ts` 的 `CommandMap` 顺序）+
2 条 `ClientNotificationMap`（`unsubscribe`/`dispatchAction`，它们不是 command 但同表路由），
14 条标 `Declined`，删掉「没有 Declined」的断言，改为「表长 == 30 + 2」。
`Declined` 因此**活了**：`createResourceWatch`/`resourceCopy|Move|Resolve|Mkdir|Request`/
`authenticate`/`sessionConfigCompletions`/`invokeChangesetOperation`/三个 automation 命令。
路由行为不变（declined 与 unknown 都回 `-32601`，已被
`declined_commands_answer_method_not_found` 钉住），但上游加命令时会编译失败——这正是
「自失效上游守卫」要防的那类漂移。（`createTerminal`/`disposeTerminal` 本轮补完，见 §H.2h。）

**2. terminal 是半声明。** `parse` 认 `ahp-terminal:/`、`Channel::Terminal` 有状态类型、
`ensure_terminal` 有路径，但 `terminal_state` 回 `None`——客户端 parse 得过、订阅只得 `not-found`。
**半声明比不声明差**（正是我们在 `titleChanged` 上反对过的那种静默不一致）。
用户裁决**补完**：`ahp_terminal_state`（`agent_server.rs`）接 live PTY 注册表，
回 AHP `TerminalState`（可见网格作为一个 `Unclassified` part、`running`/`exited` 生命周期、
`isPty`）。`claim` 报 **session** 而非 client：本宿主不在客户端之间仲裁输入，
宣告一个不会执行的 client claim 会招来两个客户端往同一个 PTY 里打字。
`createTerminal`/`disposeTerminal` 一并落到 `attach_terminal`/新的 `dispose_terminal`
（drop entry 即释放：PTY handle 在 `Drop` 里收子进程），两条命令从 `Declined` 转 `Implemented`。
实测（官方 TS client over WS）：create → 订阅拿到 **80×24 / running / isPty:true** → dispose，
三步全绿。`--no-default-features` 腿仍干净（helper 与 `terminal_state` 双臂 cfg）。

**3. 六个扩展通道声明了却不服务。** `extension_baseline` 是 trait 默认 `None`，runtime 侧无覆盖；
扩展通道 `is_state_bearing()` 为 false，所以订阅它们**只**走这条路——结果是客户端读
`_meta["x-manox"]` 看到 6 个通道，订阅任意一个收到**静默**。
修法：`fold_journal` 现在收集 `x-manox*` 侧的行（此前直接丢弃），按通道用
`ext::reducer::apply` 折成 `XManoxState`，`extension_baseline` 把它作为
**`x-manox/baseline` 通知**交付（方法名已进 `_meta["x-manox"]` 声明，客户端猜不出来）。
实测该基线是**真 fold 而非空占位**：种一条 `PlanModeChange{enabled:true}` 的 journal，
基线回 `planMode: true`。
**实施期新增的一条区分**：`x-manox-workspaces://` 与 `x-manox-commands://` 是
**连接级目录**（无 session id），与四个 per-session 通道不是一类；
`ext::is_session_scoped_channel` 把两者分开，目录通道走 `catalogue_baseline`
（workspaces 读 `known_projects`，commands 暂回空列表）。

### H.3 尚未落地（续做清单，按 plan 的 W2→W3→W4→W5 顺序）

### H.2i W4 准入账的第二轮补齐（2026-09-24）

第二份验收意见（`/private/tmp/ahp-w3-review-and-next-2026-09-24.md`）指出四项，逐项处置：

1. **四个 host→client 请求的调用点（已接）**。`route_session_capability` 现在先问 AHP：
   `route_ahp_capability` 经 `AhpRuntime::has_capable_client` 判断该会话是否有**声明过**该能力的
   AHP 订阅者，有则 `Host::request_client`，无则回退 v2 腿。
   **这是传输选择而非兼容层**：v2 删除时第二条腿随之消失，第一条不变。
   两处 fail-closed 已钉住：无合格声明者不误选本传输（`capability_routing_declines_when_no_ahp_client_declared_it`）；
   只有声明的能力才选中该连接（`manox-ahp` 的 `only_declared_client_requests_select_a_connection`）。
   **纠正审计者笔误**：第四个方法是 `x-manox/invokeTool`，不是 `invokeClientTool`。
2. **pin + 手工排序（已补，走 `x-manox`）**。见上「W4 准入账」条。
3. **`refs/agents/*` 14 个残留 ref（已清）**。删除前独立复核三条零风险依据：
   `git log main --grep="^Agent host session"` = 0、`git ls-remote origin 'refs/agents/*'` = 0、
   无分支引用。清理后主 checkout `git status` 干净、`main` 仍在 `f80f38a`。
   **复发风险仍在**：hcode 当前分支 `experiment/manox-agent-host` 仍指向本仓运行，
   只要它继续在本仓做 checkpoint 就会再累积——需在 hcode 侧关闭 checkpoint 或让它离开本仓（仓外决定）。
4. **push + PR（已完成）**：PR #818，26 个提交，base `main`，head `ahp-host`（按用户裁决不用 `codex/` 前缀）。

### H.2j W4 前置核实：计划的一条前提不成立（2026-09-24）

W4 开工前逐文件核实，发现**计划 §一「`manox-session-core` 整体删除」不成立**：

`crates/manox-session-core/src/agent_server.rs`（5,913 行 / 91 个方法 / 261 处 v2 引用）是
**同一个 struct** 同时承载两件事：

- **v2 网关**（~2,500 行，该删）：`handle_call`(266) / `handle_note`(256) / `route_call` /
  `route_capability_call` / `ReplyCtx` / waterfall delivery / `fail_closed` / `apply_reply`；
- **运行时 intent**（该留）：AHP 适配器需要的约 20 个方法
  （`create_session_request`/`fork_session`/`submit`/`steer`/`dispose_session`/`session_thread`/
  `archive_thread`/`set_model`/`set_cwd`/`rename_thread`/`pin_session`/`order_session`/
  `terminal_input`/`terminal_resize`/`ahp_terminal_state`/`terminal_raw_tap`/`compact`/`plan_seed`）。

**拆分可行的依据（实测）**：`src/ahp/` 对 v2 wire 层（`handle_call`/`handle_note`/`route_call`/
`ReplyCtx`/delivery）的引用数 = **0**；对 `AgentServerInner` 内部状态（`sessions` 等）的直接访问 = **0**，
只经方法。故窄接口足够。

**但拆分不是「按方法切」**：AHP 需要的方法**间接**调 v2 通知函数，须**换出口**（改为 AHP publish）
而非移植：

| AHP 方法 | 调用的 v2 函数 |
|---|---|
| `create_session_request` | `route_host` / `route_note` / `replay_pending_adjudications` |
| `submit` | `note_error` |
| `dispose_session` / `archive_thread` | pump / streams / deliveries（v2 专属） |

**另核实两点**（均与计划有出入）：
- `RpcError` **不需要**搬——`manox-ahp` 用的是 AHP 自己的 `JsonRpcError`，而 AHP 适配器对
  `RpcError` 引用数为 **0**。（计划把 `RpcPeer`/`MsgId` 列了，`RpcError` 未列，实测它属 v2 侧。）
- `manox-protocol` 里 `journal`/`base64_bytes` **已是** `manox-journal` 的 re-export（W0 已搬），
  `journal_stream.rs` 与 `wire.rs` 的外部消费者 = **0**。

**结论**：W4 是一次「5,400 行删除 + 6,000 行文件拆分」，不是计划设想的三处快删。
拆分后 `manox-session-core` 只剩 v2-free 的 gateway/journal_query/translate/waterfall 四块，
名字已不诚实——但改名/并入会进一步扩大本次破坏面，故排 W4 之后单独收口。

### H.2h 版本注记

- 本轮新增 `x-manox/baseline` 通知（扩展通道基线），已进 `_meta["x-manox"]` 声明。
- **terminal 快照面已服务**（`createTerminal`/`disposeTerminal` + `TerminalState` 快照）；
  `terminal/input`、`terminal/resized`、`terminal/data` 三个 action **未接**——客户端能建能看
  但不能打字、不能改尺寸、收不到活输出。W4 前必补。
  （初稿此处写「terminal 项已完成」，**该结论超前**：实现描述只覆盖快照，结论却把
  「快照已服务」表述为「terminal 已完成」。已按实测订正。）

0. **W2 状态：收口**（2026-09-23）。§G 的 W2 五条门禁逐条对照：
   ① 收敛性证明（`translation_convergence.rs`，含 §H.2 那次抓到真缺陷的轨迹）——绿；
   ② 全射/覆盖门（`translate/mod.rs` 的 `target_of` 穷举 + 编译期门）——绿；
   ③ `快照 == fold(重放)`（L10）——绿；
   ④a 官方 TS client over WS（§H.2e）——绿；④b 独立立项（见下）；
   ⑤ 错误码纪律（`error.rs` 的 `DECLARED` 表 + 各 refusal 路径的测试）——绿。

   **W2 认完成**（验收公式见下）。

   ### 验收公式（取代「AHP 覆盖度」）

   > **v2 今天能做的，AHP 面都能做 → 才能删 v2。**

   AHP 30 个命令中 14 个对应 manox 不具备或不需要的能力（automation / OTLP / MCP Apps /
   OAuth / resource-watch / changeset），按规范结构性拒掉（`-32601`）**即正确答案**，不是欠账。
   追求 30/30 会把 manox 变成 AHP 参考实现。

   按此公式的 W4 准入账（2026-09-24 复核）：
   - ✅ `extension_baseline`（§H.2g）
   - ✅ 四个 `x-manox` server→client 请求（§H.2i）
   - ✅ `terminal` 写路径 `input`/`resized`/`data`（§H.2i）
   - ✅ 客户端贡献工具（`session/activeClientSet` → 既有 `embedder_tools` 路径，§H.2i）
   - ✅ 四个 host→client 请求的**调用点**（`CapabilityClient` 的 browser/clipboard/
     openExternal 经 `route_ahp_capability` 优先问 AHP，无合格声明者则回退 v2 腿；见 §H.2i）
   - ✅ pin + 手工排序 —— **走 `x-manox`**（按「按能力查而非按命令查」的方法论核实了 AHP 全部
     候选槽位：`SessionStatus` 位集 / `SessionMetadata` / `SessionSummary` / 全树
     `pinned|sortOrder|displayOrder|reorder`，确认**无落点**）。
     pin 写 store 行 + sidecar（与 v2 `PinThread` 同一层），且 journal 的 `PinnedArchived`
     早已被 translator 折成 `x-manox/pinnedChanged`，读回路径本来就通；
     排序写 `sidebar_order`，读回经 `x-manox-workspaces://` 基线的 `order`（文件夹序 +
     每个 partition 的线程序）。
     **实施期发现**：排序**不进 journal**——`sidebar_order` 是独立 durable 文件，v2 侧靠
     `ThreadsUpdated` 快照广播。故 translator 无可发射的排序事件，排序是「客户端写 + 通道读回」
     的状态，这与 pin 不同（pin 有 journal 行）。
   - ⬜ `x-manox/fetchEntries` —— v2 在服务，W4 前需补齐或明确接受降级
   - ⬜ `resourceResolve`/`resourceCopy`/`if_match`（基础围栏已在；v2 未服务这三条，故非阻塞）

1. **④b（独立立项）**：真 VS Code Agents window 的方言税（R5 五处）。前置条件是 hcode fork 里
   加一条可插拔 connection 来源（现只有 ambient/ssh/wsl，全是起 VS Code 自己的 agent host）。
2. **W3**：manox-app 侧的 `AhpStore`（`ahp::Client` + reducers）、进程内 `ahp::Transport`、
   `ConversationState` 改由 `ChatState.responseParts` 派生、删 `client_store*`/`journal_fold`/
   `journal_translate`/`server_note_translate`、重写 `source_gates.rs`、更新 `UI-MAP.md`。
3. **W4**：按 §F 删除清单拆除 v2；grep 门禁（生产区零 `FromClient|FromServer|ClientCall|
   ClientNote|ServerCall|ServerNote|PROTOCOL_EPOCH|StreamFrame`）；`manox-napi` 改 AHP；
   `~/.manox/gateway-ws.json` → `ahp-ws.json`、`cx web` → `cx ahp`。
4. **W5**：有界重放缓冲（按 `serverSeq` 重放替换快照腿）、`delivery.maxLatencyMs` 合并调优、
   `resource*` 完整面、远程鉴权、多客户端并发与 owner 语义。

### H.2f ④a 外接客户端实测：三个只有外部 client 能暴露的发现（2026-09-23）

④a 的价值在执行期立刻兑现——三个发现都不是 Rust 套件能看见的：

1. **`listSessions` 对真实规模不可用（已修，性能缺陷）**。原实现对**每个** session 折一次完整 journal
   （`session_summary` → `block_on(session_state)`）。本机 401 个 session / 1.2 GB，于是
   `listSessions` **超过 8 秒超时**——而它是客户端第一个调用、也是后续每一页的来源。改为从
   **store 行**构建（与 v2 `threads_snapshot` 同一口径），实测 **401 个 session 上 1 ms 返回 27 条**
   （27 = 活跃 18 + 未 superseded；归档 222 条不进列表，与 v2 侧栏一致）。store 行没有的
   三个字段（status/activity/workingDirectories）只在**已 seed** 时填真值，否则按
   `is_running` 给 Idle/InProgress 并留空——绝不为了三个字段去折一份 journal，也绝不编造。

2. **重命名的「成功」是假的（已修，fail-open）**。用外接客户端对**真实会话**（78 MB，正被
   Manox.app 以写租约驱动）执行 `session/titleChanged`：journal 行**没落**、sidecar **没变**，
   而宿主回了 `Accepted`。成因有两层：
   - `dispatch_store_journal_row` 在**冷路径**遇到外来租约时按设计**响亮跳过**
     （`tracing::error!` + 「sidecar carries the flag」，`engine.rs:853`），但 store 侧
     `rename_thread` 返回 `bool`，把「标题空白」和「没能持久化」**折成同一个 `false`**；
   - dispatch 又把每个 `false` 报成「a session title cannot be blank」——**对合法标题说了假话**。
   修法：引入 `RenameOutcome{Renamed, Blank, UnknownSession, NotPersisted}`；
   `dispatch_store_journal_row` 返回「这行有没有落点」（活 actor 队列 或 文件存在），
   `journal_title` 据此返回是否可持久化，dispatch 四种结局**各自给各自的 reason**。
   实测复验：对非本进程拥有的会话现在回 `unknown session: <id>` / `… not writable from this
   process`，对合法且可写的会话回 `Accepted` 且 journal 行确实落盘。
   **第三层（实测追加）**：外接客户端跑完整 `createSession → subscribe → rename` 时又暴露两处
   ——`createSession` 播种的**新会话**还没有 store 行（行要等 list refresh 扫到新文件），
   于是 `seeded` 在 `session_state` 上 `?` 掉、对这个刚建好的会话回 `session not found`；
   而 store 侧只按 `summaries` 认会话，把「按 path 认识但没有 summary 行」读成「unknown session」。
   两处都修（chat 半边本来就有这个宽容，session 半边补上；store 改为 row **或** path 认得即可）。
   最后把 `journal_title` 的前置收紧为**文件必须已存在**：仅有活引擎 route 不够——
   一个从未 materialize 文件的引擎收到排队的 `AppendJournal` 会**无声丢弃**，
   于是「已入队」被误报成已持久化。修后该场景回
   `the session's journal is not writable from this process`（fail-closed），
   而 journal 已存在的会话正常 `Accepted` 且行确实落盘。
   **这也是「fail-closed 不是口号」的一个实例**：一个 fail-open 的写回执比拒绝更糟，
   因为它会让每个订阅者折进一个永远无法 replay 的标题。

3. **`createSession` 的 `activeClient.tools` 是上游 crate 与规范客户端的真实分歧（未修，待决）**。
   实测帧：`{"channel":"ahp-root://","session":"…","activeClient":{"clientId":"capture"},
   "workingDirectories":["file:///tmp"]}` → 宿主回 `-32602 invalid params: missing field
   \`tools\``。核查：`SessionActiveClient.tools: Vec<ToolDefinition>` 在 pin 的
   `ahp-types 0.9.0` 里**没有 `#[serde(default)]`**（必填），TS 类型 `state.d.ts:241` 同样是必填
   `tools: ToolDefinition[]`；但**规范自己的 TS client** 在 `createSession` 里不发这个字段
   （`createSession` 走 `CommandMap` 泛型通道，TS 的 `activeClient` 拼装不填 `tools`）。
   即：**类型要求它、官方客户端不填它**。宿主拒绝是「按 pin 的类型」正确，但会让标准客户端的
   正常建会话失败——属 R2/R5 类。处置选项（未定）：(a) `activeClient` 缺 `tools` 时按空表接受
   （对客户端宽容、偏离 pin 类型）；(b) 记入 `test/upstream-workarounds` 式自失效守卫，等上游修
   TS client。**倾向 (a)**，因为它与既有的「reference client 宽容」先例一致（`/ahp` 的
   token/origin 闸已为 VS Code 放宽过），但这条留待用户裁决。

### H.4 AHP 上游文档漂移（实施期实测）

- `docs/specification/*.md` 的示例仍写 `"0.3.0"`，而 `ahp-types::version::PROTOCOL_VERSION` 为 `0.9.0`
  ——实现以 `types/` 与 crate 为准。
- `JsonRpcRequest.id` 在 `ahp-types` 里是 `u64`：使用字符串 id 的 JSON-RPC 客户端会在解析期被拒
  （整帧判为 `-32700`）。官方 Rust/TS 客户端均发数字 id，故暂不处理；容忍字符串 id 记入 W5 候选。
- 规范 §B.4 提到的 `PlanVerdict` 在 `manox-protocol` 里**不存在**（实测 `ServerCall` 只有 6 个变体），
  计划评审经 `Approve` 腿到达；`x-manox-plan/verdictRequested|verdict` 仍按 §D 声明保留。
