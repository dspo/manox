# manox ↔ manox-app 的 Agent Host Protocol（AHP）架构

> **权威优先级**：AHP wire 类型与 reducer 以 `ahp` / `ahp-types` crate（pin `=1.0.0`）为准，
> 本文只描述 manox 宿主半边的架构、通道面与不变式；上游文档仅作语义参考（其示例仍写 0.3.0、
> `annotations` 行缺失、`disposeChat` 有矛盾）。
>
> **依赖方向**：`manox-ahp` → `manox-ahp-runtime` → `manox-session-core`。`manox-agent`
> 永不认识 AHP；协议适配一律在其上层。

## §A 分层与终态

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
   ├─ x-manox 扩展面（plan / work / metrics / thread / workspaces / commands）
   └─ resource* 最小面（工作目录围栏内）
           ▲ journal 条目全射翻译
manox 内核 ThreadCore + Journal v4（磁盘 .jsonl，生态工具仍可直读）
```

一句话：**journal（磁盘 .jsonl）是唯一 durable 权威；`manox-ahp` host 把 journal 追加全射翻译成
AHP 动作并广播，AHP 通道状态是该 journal 的确定 fold；manox-app 是用 `ahp` crate
（client + reducers）的纯 AHP 客户端，另经同一 in-proc `Transport` 抽象走进程内腿。**
L3–L12 不变式的逐条现状见 §C。

三条结构性规则：

- `manox-ahp` 不认识 manox 运行时：它的 `Backend` trait 是唯一窗口，测试可用脚本 backend 驱动。
- `manox-ahp-runtime` 是 `Backend` 的真实实现侧缝；它不命名具体的会话运行时（builder 由拥有
  会话存储的一方安装），也不反向依赖会话核心。
- `manox-session-core` 实现 `SessionRuntime` 缝；写操作 fail-closed —— AHP 的 action 已被 reducer
  折叠进订阅者状态，运行时拒绝必须回报为 refusal，静默 no-op 会让每个客户端收敛到从未发生的状态。

## §B 通道与动作面

### B.1 通道 / URI

| AHP channel | 映射到 manox | 说明 |
|---|---|---|
| `ahp-root://` | 全局 | `agents[].models`、terminals 目录、host config |
| `ahp-session:/<uuid>` | **thread**（侧栏一行） | title / project / `workingDirectories` / status 位集 / `chats[]` 目录 / `defaultChat` = thread 的 active-session 指针；pin、分组、手工排序落 x-manox |
| `ahp-chat:/<cid>` | **session**（一份 `.jsonl` journal） | turns / activeTurn / responseParts / toolCalls / pending / draft；`createChat{source:{kind:'fork'|'sideChat'}}` ↔ 分支/fork/leaf 重定向 |
| `ahp-terminal:/<id>` | terminal | `data`/`input`/`resized`/`claimed`/`exited`；多客户端同权 |
| `x-manox-plan:/<chat-id>` | 执行计划文档 | `plan_update` 的投影 |
| `x-manox-work:/<session-id>` | goal / 后台任务 / browser suites / 子代理树（depth/branch/agent_label）与常驻 child-run 名册 | AHP 缺口补齐 |
| `x-manox-metrics:/<chat-id>` | 会话聚合 | turns/messages/per-model usage/成本/context%/git stats，按 cursor 变更推送 |
| `x-manox-thread:/<session-id>` | thread 行 | pin、label、session info、leaf 游标 |
| `x-manox-workspaces://` | `manox-workspace` 域 | 目录行 + 有序会话账 + 全局 archive 集 |
| `x-manox-commands://` | 命令/skill 目录 | stateless 快照 + 变更通知 |

### B.2 帧与传输

| AHP 机制 | manox 宿主侧落点 |
|---|---|
| JSON-RPC 2.0 | request/response；宿主→客户端 `action` 通知；客户端→宿主 `dispatchAction` 通知；per-channel 协议通知；宿主→客户端 request（`x-manox/*` 客户端能力） |
| JSON-RPC `id` | 请求关联；写路径以 `clientSeq` 乐观对账，回执 = 动作回声 + `serverSeq` + `origin{clientId,clientSeq}` |
| `subscribe`/`unsubscribe` + `action` 信封（`serverSeq`） | 溢出 = 断连 → `reconnect`（快照腿）；高频 delta 用 `subscribe.delivery.maxLatencyMs` 合并 |
| `subscribe` 结果 `{resource,state,fromSeq}` + `view.turns` + `fetchTurns{cursor}` | 会话快照与 turn 级分页；没有 entry 级翻页通道 |
| `initialize{protocolVersions,clientId,clientInfo,initialSubscriptions,locale}` → `{protocolVersion,serverSeq,snapshots,_meta:{"x-manox":{…}}}` | 版本协商 + 扩展声明 |
| AHP 标准错误码 + x-manox 码区间 | 声明表单一事实源：生产零无码 |
| 进程内 `ahp::Transport` impl | typed 帧，不序列化；in-proc 无界，绝不阻塞发布方 |
| axum `WebSocketUpgrade` | loopback `/ahp`，`?token=` + Origin 门 + 单例 `gateway.lock`；端点文件 `~/.manox/gateway-ws.json` |

### B.3 命令与动作面

**客户端命令（host 服务）**：`initialize`、`subscribe`、`unsubscribe`、`ping`、`reconnect`、
`listSessions`、`createSession`、`disposeSession`、`createChat`、`disposeChat`、`fetchTurns`、
`x-manox/compact`、`x-manox/goal`、`x-manox/openTurn`、`x-manox/teardown`，以及
`resourceRead`/`resourceWrite`/`resourceList`/`resourceDelete`（只落在会话工作目录子树 +
granted-root 围栏内）。

**客户端可派发动作（`dispatchAction`，只接受声明的子集）**：

- `chat/turnStarted`、`chat/pendingMessageSet{kind:'queued'|'steering'}`、
  `chat/pendingMessageRemoved`、`chat/turnCancelled`、`chat/toolCallConfirmed`、
  `chat/inputCompleted`、`chat/inputAnswerChanged`；
- `session/configChanged`（canonical model 串、ConfigSchema 枚举 effort/accessMode；
  durable 权威 = journal `model_change` / `thinking_level_change` / `access_mode_change` 行，见 §F.3）、
  `session/isArchivedChanged`、`session/workingDirectorySet|Removed|Replaced`、
  `session/activeClientSet`（客户端工具腿，见 §D.4）；
- `x-manox/pinnedChanged`、`x-manox/orderChanged`、`x-manox/archivedChanged`、
  `x-manox/aliasChanged`。

**宿主发射动作**：

- root：`root/agentsChanged`、`root/sessionAdded|Removed|SummaryChanged`；
- session：`session/ready`、`session/configChanged`、`session/workingDirectory*`、
  `session/isArchivedChanged`、`session/activeClientSet` 回声；
- chat：`chat/turnStarted`、`chat/turnCompleted`、`chat/toolCallReady`（`options[]` +
  `_meta.x-manox.{authId,summary,deliveryId}`）、`chat/toolCallConfirmed`、`chat/error`、
  `chat/responsePart`、`chat/inputRequested`、`chat/inputAnswerChanged`、`chat/inputCompleted`、
  `chat/inputNeededRemoved`、`chat/pendingMessageSet|Removed`；
- terminal：`terminal/data`、`terminal/claimed`、`terminal/exited`；
- x-manox：`x-manox-plan/planChanged`；`x-manox-work/goalChanged`、`backgroundTasksChanged`、
  `childRunsChanged`、`browserSuitesChanged`、`activeToolsChanged`；`x-manox-metrics/changed`；
  `x-manox-workspaces/baseline|changed|removed`；`x-manox/baseline`；
  `x-manox/pinnedChanged`、`orderChanged`、`archivedChanged`、`aliasChanged`、`labelChanged`、
  `sessionInfoChanged`、`leafChanged`。

**宿主→客户端请求（server requests）**：`x-manox/browserOp`、`x-manox/clipboardRead`、
`x-manox/openExternal`、`x-manox/invokeTool`，按客户端 `capabilities` 路由，fail-closed。

**审批与询问是状态，不是 waterfall**：`Approve` → `chat/toolCallReady` + `chat/toolCallConfirmed`；
`AskUserQuestion` → elicitation 三动作；plan 评审走通用 ask（§F.6）。多 owner 汇聚策略留在宿主侧，
线下只暴露单一裁决结果与 `_meta.x-manox.deliveryId` 对账面。

### B.4 AHP 原生覆盖的面

以下能力由 AHP 自身承载，不另设自有协议面：初始化与能力声明（`initialize` 协商 +
AHP registry）、客户端缺口修复（`reconnect` 快照腿）、状态推送（channel state + `action`
信封）、审批/询问（状态化动作）、列表增量（`root/session*` 增量而非全量快照）、
能力协商（`clientCapabilities`）、流溢出（断连重连）。

### B.5 AHP 无对应 → `x-manox` 扩展面

plan 执行计划文档；goal；compaction（journal 重写，AHP 无）；子代理层级（AHP 是扁平 worker chat，
无 depth/父子树）；thread pin + 手工排序 + 分组 + alias/label；workspace 行域；后台任务；
browser suites；命令/skill 目录；会话聚合（Q 面）；thread 游标信息。以上全部落 `x-manox/*`
（合法 `x-` 前缀 + `_meta`），第三方 host 下优雅降级。

## §C 不变式

| 不变式 | 现状 |
|---|---|
| **L3 一切状态变更皆 journal 条目** | 原样：AHP 动作面是 journal 的投影，不是新权威；「字段变了但无事件」仍结构不可能 |
| **L4 seq 单点盖章** | 双层：journal 内每条链的 `seq` 仍由单一 append 点盖章（磁盘/回放契约）；网关另有全局 `serverSeq` 在动作派发点单点盖章 |
| **L5 快照不丢、溢出即重同步** | snapshot 永不丢（subscribe / reconnect 快照）；高频 delta 用 `delivery.maxLatencyMs` 合并；网络载体溢出 = 断连 → 客户端 `reconnect` 取快照；进程内载体无界 |
| **L6 客户端零领域 fold** | AHP reducer 是客户端唯一 fold，视图不得解析 provider/工具词汇 |
| **L7 写只回回声** | 写是 `dispatchAction` 通知；回执 = 动作回声 + `serverSeq` + `origin{clientId,clientSeq}` 乐观对账；运行时拒绝是显式 refusal |
| **L8 wire 身份 canonical** | `AgentInfo.provider` + `SessionModelInfo.id` = canonical `provider/model` 串 |
| **L9 UI 值经协议面表达** | AHP 通道状态 + x-manox 扩展状态 + 宿主→客户端请求；组件层禁私开同步通道 |
| **L10 重放等于内存** | 原样，并加门禁：`快照 == fold(重放)`，config 座位同样覆盖 |
| **L11 单网关** | 一进程一个 AHP host，单 `serverSeq` 域 |
| **L12 声明面即公开契约** | AHP 自身 registry + 唯一一张 x-manox 扩展声明表 + journal 词汇表（磁盘面） |

宿主侧内部约束（不属 AHP 面）：thread 句柄锁不可重入；前端能力调用一律以 future 表达，不在锁内
做异步 UI 往返。

### C.1 事件与 seq

journal 的每一行都是持久事实：消息、工具、turn 边界、配置与状态变更。envelope 键
（`seq`/`id`/`parentId`/`timestamp`/`type`）独占，payload 不复用，所以工具句柄是 `callId`、
子代理句柄是 `agentId`。`seq` 是链深（dense 0-based），由单一 append 点盖章；行序可以因并发
写者交叠而偏离链序，内存一律按链排序，读回不依赖行序。网关的全局 `serverSeq` 是另一个域，
只盖 AHP 动作。

### C.2 journal 词汇

- 唯一词汇是 `session::event::Event`（serde tag = 行名）；没有别名、没有第二套 schema。
- 用户可见且需重启后存续的事实必须是事件；身份字段永不出现在事件 payload（事件只带 id 引用）。
- 本 build 不写的行名按未知行拒收（`unknown_event_types_are_refused_not_guessed`），没有兼容读。
- 身份只在 journal 首行 header 出现一次，此后永不改写；血缘（parent/toolCallId/runId/depth）是
  header 字段，不是可变旁册指针。

### C.3 v4 读面

- 磁盘只有一个格式版本（`FORMAT_VERSION = 4`）；别的版本在 open 时以可操作的错误拒绝，
  文件原样不动（转换是一次性 operator migration，运行时从不重写）。旧版本用
  `python3 script/migrate-journals.py`（默认 dry-run，`--apply` 执行，`--backup` 指定备份；
  同时把平铺 `sessions/` 归位到 core 布局、把命令文件里的 `$ARGUMENTS` 改写为
  `{{ arguments }}`）。未来版本（`found > 4`）不给转换建议，要求升级二进制。
- 打开校验：header 必须合法；每条链的 parent 必须存在、无环、`seq` 等于链深；坏文件报错而不是
  猜一种修复。
- 读接口：`journal_range` 以链序返回记录；活动 leaf 是**最后写入行**的 cursor（通常是文件序
  最后一行），导航后再 append 时它会与链序最后一行不同——恢复必须按写入尾而不是链深尾取
  cursor。header-only 文件可被列表扫描有界读取（不加载 transcript）。

## §D x-manox 扩展面（单一事实源，随 `initialize` 协商）

### D.1 声明块

```jsonc
"_meta": { "x-manox": { "version": 6,
  "channels": ["x-manox-plan:/", "x-manox-work:/", "x-manox-metrics:/",
               "x-manox-thread:/", "x-manox-workspaces://", "x-manox-commands://"],
  "clientDispatchableActions": ["x-manox/pinnedChanged", "x-manox/orderChanged", "..."],
  "commands": ["x-manox/compact", "x-manox/goal", "x-manox/openTurn", "x-manox/teardown"],
  "serverRequests": ["x-manox/browserOp", "x-manox/clipboardRead", "x-manox/openExternal", "x-manox/invokeTool"],
  "capabilities": { "multipleChats": {"fork": true, "sideChat": true},
                    "multipleWorkingDirectories": { "immutablePrimary": false } } } }
```

### D.2 扩展通道与其承载的领域状态

| 扩展通道 | 承载的领域状态 |
|---|---|
| `x-manox-plan:/<chat-id>` | 执行计划文档（`planChanged` ← journal `plan_update`）；plan 开关与 plan 评审不存在 |
| `x-manox-work:/<session-id>` | goal、后台任务、browser suites 运行态、子代理树（depth/branch/agent_label）、常驻 child-run 名册 |
| `x-manox-metrics:/<chat-id>` | 会话聚合：turns/messages、per-model usage、成本、context%、git stats，按 cursor 变更推送 |
| `x-manox-thread:/<session-id>` | thread 行：pin、label、session info、leaf 游标 |
| `x-manox-workspaces://` | `manox-workspace` 域：目录行 + 有序会话账 + 全局 archive 集 |
| `x-manox-commands://` | 命令/skill 目录：stateless 快照 + 变更通知 |

### D.3 命名与共存规则（normative）

- **单一事实源**：一切 `x-manox` 命名（通道、动作、命令、serverRequest、`_meta` 键、错误码区间）
  必须声明在本文这一张表（及其 D.1 声明块）里；代码中不得出现未声明的 `x-manox` 名。新增 = 先改本文，再改代码。
- **未知容忍**：对端收到未知扩展通道/动作/`_meta` 键时忽略、不断连、不报错。
- **第三方 host 优雅降级**：manox-app 连非 manox host 时，`_meta["x-manox"]` 缺失即隐藏对应 UI
  能力面，绝不因扩展缺失而报错或降格核心流程。
- `x-` 前缀与 `_meta` 是 AHP 1.0.0 明示的合法私有扩展位；`x-manox` 不承诺第三方 host 支持。
- **声明即承诺**：只有真正有实现的动作/命令/通道才列入声明表；未实现的一律拒绝（不是静默 no-op）。

### D.4 客户端工具

AHP 原生 `session/activeClientSet` 携带 `activeClients[].tools`：注册即动作，宿主 reducer 把它
折叠进订阅者状态，同时落到引擎的 `embedder_tools` 注册表——一条注册路径，不是两张表。完整替换
按 `clientId` 幂等；没有活引擎的会话拒绝注册（否则模型会拿到一个无法运行的工具）。

### D.5 宿主事件推送

- `Models`：provider reload 时立即推 `root/agentsChanged`（不是等下一次列表）。
- `ThreadsUpdated`：任何 summary 写（title auto-stamp、interacted_at、pin/archive/tag、rescan）
  都推给每个连接，客户端无需 in-process store 订阅。
- `SessionStatus`：由 session/chat 的 `status` 位集派生，不设独立事件。
- `SessionCreated` / `SessionDisposed`：`root/sessionAdded|Removed` + `session/chatAdded`。

### D.6 生命周期与恢复

- **dispose 不写 durable 标记**：未物化会话 = 内存删行 + `root/sessionRemoved`；已物化会话仍在
  列表里，日志里没有「已处置」行。
- **订阅但从未提交的会话**：settle 可以冷开它（verdict 到来时引擎才物化），journal 行落在
  seed 的 watermark 之上，客户端 fold 因此看到卡片关闭。
- **restore 对账**：链尾是未闭合 turn（`turn_start`，或无 `turn_start` 的 user 行）时，
  restore 追加 `cancelled: true, failed: false, reason: "host exited"` 的 `turn_finish`，让
  `快照 == fold(重放)` 成立。
- **共享 journal 的 turn owner**：`turn_start` 行携带进程 pid；owner 死掉时读者可以安全结算，
  活着的 owner 的 turn 永不被自动取消。

## §E 依赖与选型

### E.1 依赖

AHP 只发客户端 SDK（`ahp` / `ahp-types` / `ahp-ws`），宿主半边（JSON-RPC 路由、
`serverSeq`、快照/重连、动作校验与派发、`resource*` 文件面）是 manox 自建的面。
`ahp-types`（wire 类型）+ `ahp`（client + reducers）**精确 pin（`=1.0.0`）**；`ahp-ws` 进
manox-app 侧测试与外部客户端 e2e（宿主侧 WS **不用** `ahp-ws`，它只能 dial；宿主用 axum
`WebSocketUpgrade` + 自写 `ahp::Transport` impl——架构参考，禁止复制其代码）。

### E.2 crate 布局与单网关

`crates/manox-ahp/` 承载 **AHP 协议的宿主半边 + journal→AHP 翻译 + x-manox 扩展声明面**；
`Backend` trait 缝由 `manox-session-core` 实现。`manox-ahp-runtime` 是运行时半边
（`SessionRuntime` 缝 + 适配器）。同一 axum listener 只留 `/ahp`；token 与 `gateway.lock`
单例纪律不变，端点文件 `~/.manox/gateway-ws.json`。

### E.3 会话聚合（Q 面）

`ConversationInfoCache` 按 `(thread_id, cursor)` 缓存 journal fold：turns/messages、per-model
usage、累计 token 与成本、context%、git stats（host lookup，缺失为 `null`）。它是
`x-manox-metrics` 通道的读模型，不是独立权威。

### E.4 协商与版本

`protocolVersions:["1.0.0","0.9.0"]`（caret 语义，选择逻辑委托
`ahp_types::version::negotiate_protocol_version`）；扩展经 `_meta["x-manox"]`；只 advertise
并实现真正用到的动作子集，未实现的动作一律拒（`rejectionReason` 或 x-manox 稳定码）。

### E.5 错误码

AHP 标准码 + `x-manox/*` 码表（单一声明表；生产零无码的结构门禁）。

### E.6 能力

advertise `multipleChats{fork,sideChat}`；`multipleWorkingDirectories`（不开 `immutablePrimary`）。

### E.7 durability

AHP 无持久化要求，journal 仍是唯一 durable；宿主启动时由 journal fold 得到通道状态，
AHP 快照 = 该 fold 的输出。

## §F 实现事实（durable 行为）

### F.1 journal 追加与 leaf

- 单一 append 点在链上分配 dense `seq`；并发写者由文件 fence（flock + stale-view reload）
  线性化。行序可以与链序不同，读回按链排序；活动 leaf 取最后写入行的 cursor（§C.3），链序
  最后一条在导航后可能不是它。
- 新格式只加行类型，不改 envelope；旧格式文件在 open 时拒绝（§C.3），不做运行时转换。
- 追加失败不改变内存链；durable 行先于对外广播（客户端看到的动作都以已落盘行为前提）。

### F.2 echo retirement

`Submit` 携带的 `origin_rpc` 在 accept 时钉到用户行；宿主的 durable 行以客户端消息 id 落盘，
客户端据此 retire 乐观 echo（`user_entry_id`）。steer 行没有 origin pin（origin 只属 submit
路径），其身份是 journal 行 id 本身——见 §F.5。

### F.3 session config 三键的 durable 权威是 journal

- 行类型：`model_change`、`thinking_level_change`（wire 字符串）、`access_mode_change`
  （kebab wire 词汇 `sandbox`/`full-access`）。`replay.rs` 的宿主 fold 与 `projection.rs` 的
  `target_of` 全射门同步登记。
- 生产者：`model` 在 `AgentSession::set_model` 的单一 choke point 落行；`reasoningEffort` 的两个
  `SetThinkingLevel` 臂收敛到同一落行词汇（中段臂经 live serializer task 落行，绝不在 select 臂内
  await append 锁）；`accessMode` 在 `SetAccessMode` 臂落行。三处都带「仅真实变化」守卫，守卫读
  最后 journaled 值而不是活镜像。
- 消费：`projection.rs` 把三行折成 `session/configChanged`；restore 从链上恢复，无行即默认值
  （不迁移、不做兼容读，用户重选一次即落行）。
- sidecar 不存 config 副本；conversation 对象行只是新建线程的 draft 意图，`sync_object_config`
  经正常 setter 派发落行。`networkAccess` 仍是 session-memory。

### F.4 create 的项目与 config

- `createSession` 的 project 解析顺序：`intent.project`（显式覆盖）→ 对象行的 belonging。belonging
  的权威是对象行，`workingDirectories` 只是授权集。deferred 与 seeded 两条路径都走
  `Thread::new_in_project`；无对象行的 create 保持 unfiled。claim 的 Reuse 早退在解析之前，
  已物化会话绝不因解析重铸或迁移。
- `CreateSessionParams.config` 的三键（`model`/`reasoningEffort`/`accessMode`）先整单校验
  （未知键、非字符串值拒绝整个 create；model 串经注册表解析，失败以 `model/unresolvable`
  拒绝），全部发生在任何状态创建之前；通过后由既有 choke point 为真实变化落行，deferred
  会话的行随首条 assistant 消息物化。
- 中段排队的 `SetModel` 同时入队 `PendingMutation::Model`，由 turn 边界或 settle 的
  `flush_pending_mutations_inner` 落 `model_change` 行，行先于同一 `TurnUpdate` 写入下一请求；
  写失败即中止该 run。idle 路径仍走 `set_model` 的 choke point，不产生第二行。

### F.5 mid-run steer 折成 turn 分区

`projection.rs::on_message` 的 user 行按 turn 状态分三路：

1. **无 turn 打开** → `chat/pendingMessageSet{kind:'queued'}`，进 `pending_user`，由下一个
   `turnStart` 作为开场消息携带。
2. **turn 已开、尚无开场消息**（deferred create 的形状）→ 该行是该 turn 的开场消息，以同
   `turnId` 重发 `chat/turnStarted`。
3. **turn 已开、已有开场消息** → 先按 turn 边界结清当前 turn（`chat/turnCompleted`，
   Outcome=Complete），再以该 journal 行开新 turn：
   `chat/turnStarted{turnId:'t-<entry.id>', message: 行文本, queuedMessageId: <entry.id>}`。
   该行不再发 `pendingMessageSet`，也不进 `pending_user`。

- 身份就是 journal 行 id：客户端 steer 行以客户端 id 落盘，与它
  `pendingMessageSet{Steering}` 用的 id 相同；reducer 在 `chat/turnStarted` 上按 id 清槽。
- 边界 `chat/turnCompleted` 携带 `_meta.x-manox.steerBoundary = true`；客户端按「完成」触发的
  run 级动作必须跳过该边界。一个 run 可以产出多个 turn。
- parked steer 在下一个 run 的 turn 1 被消费时同样走第 3 路（`OpenTurn.message_text` 记录
  `turn_start` 携带的开场文本，steer 行不会覆盖 prompt）。
- run 在同一条 journal 行上继续；steer 行只落在内核回合边界，边界 `finish_turn` 对未结算
  tool call / 未回答 ask 的 settle 是防御性规则，不是取消活工作。

### F.6 plan 是内置 skill，不是模式

- 没有 plan mode：harness 不持有 plan 开关，审批门没有 plan 文件豁免，`~/.manox/plans` 不是约定。
- journal 只有 `plan_update`（`UpdatePlan` 的执行计划快照）；`plan_mode_change` / `plan_request` /
  `plan_review` 不是词汇，出现即按未知行拒载。
- `/plan` 是内嵌技能（`skills/plan/SKILL.md`，`disable-model-invocation: true`，仅用户斜杠入口；
  磁盘同名 skill 遮蔽内置）。`/plan <task>` 走 `submit_skill`：渲染契约正文 + 追加 task，
  display 文本保留 `/plan <task>`。
- 契约要求以单问题 `AskUserQuestion` 卡提案（header `Plan review`，`detail` = 完整计划 markdown，
  `intent.kind` = `plan-review`，选项 Approve / Approve with changes / Revise），答案经既有
  turn-final 播种进入下一回合。
- 通用载体：`on_question` 投影把问题级 `detail` markdown 写进 `ChatInputRequest.message`
  （单问题原样；多问题按 `header` 分节拼接；全无 detail → `None`）。任何 AHP 客户端从 `message`
  读 ask 的支撑文本。

### F.7 硬化路线

有界重放缓冲（按 `serverSeq` 重放替换快照腿）、`delivery.maxLatencyMs` 合并调优、`resource*`
完整面（大输出 ContentRef、图片引用、客户端 `virtual://` URI、工作目录围栏）、远程与鉴权
（AHP `protectedResources`/`authenticate` 接 provider 凭据）、多客户端并发与 owner 语义、
长会话快照成本度量。门禁：`ahp-*` minor 升级演练以收敛性门禁为护栏；上游
`types/test-cases/reducers/*.json` 与我们信封的一致性抽样。

## §G Assumptions

1. `ahp`/`ahp-types` 的 reducers 可作宿主侧权威 fold（规范明示双端同码），我们**只依赖不复制**。
2. 进程内 channel 作为 AHP 传输合法，且 typed 帧（不序列化）满足一致性要求
   （`tests/ahp_transport_parity.rs` 守护）。
3. 只 advertise 并实现真正用到的动作子集；未实现动作拒绝（不是静默 no-op）。
4. AHP 客户端对未知 `_meta` 键与未知 `x-` 方法必须忽略而不致断。
5. i18n 与 UI 文案完全留在 manox-app；协议面不承载语言配置。
6. 单窗口、单 host 进程；多客户端并发共享同一 `serverSeq` 域。
