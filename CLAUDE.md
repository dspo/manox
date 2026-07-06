# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

manox 是一个进程内 native agent 工作台，按 Zed 的 agent / agent_ui / zed 三层架构复刻。基于 GPUI（Zed 的 GPU 加速 UI 框架）+ gpui-component（longbridge 组件库），LLM 通过 `~/.config/cx/cx.providers.config.yaml` 直连 provider。

## 构建与开发命令

```bash
# 构建（debug 下 gpui 默认 opt-level=3，否则渲染极慢）
cargo build

# 运行
cargo run

# 运行所有测试（live 测试未标 `#[ignore]`，而是用 `MANOX_RUN_LIVE` env 门控：
# 未设该 env 时测试早退、不发真实 API；故 `cargo test` 默认安全）
cargo test

# 运行真实 API 测试（需 macOS Keychain 或环境变量配置 API key）
MANOX_RUN_LIVE=1 cargo test

# 运行单个 crate 的测试
cargo test -p agent

# 运行特定测试
cargo test -p agent -- test_name

# 代码检查
cargo clippy --all-targets

# 格式化
cargo fmt --all
```

## 工具链

- Rust toolchain: **1.95.0**（`rust-toolchain.toml`）
- edition: **2024**（所有 crate）
- 需要 `clippy`、`rustfmt`、`rust-src` 组件

## 架构总览

```
crates/
├── agent/          # 核心逻辑：Thread 状态机 + LanguageModel trait + tools + SQLite 持久化
│                   # 不依赖 gpui-component，gpui-native
├── agent-ui/       # UI 层：Workspace + ConversationState + Sidebar + 消息渲染
│                   # 依赖 gpui-component + gpui-component-assets
└── manox/          # 薄 bin：窗口创建 + 主题 + tracing 初始化
    main.rs          # 组装 agent::init + agent_ui::Workspace + Root
```

### agent crate（`crates/agent/`）

**核心模块：**

- `system_prompt.rs` — 主 agent system prompt 构建层。`build_main_system_prompt` 注入运行时身份：thread id、cwd、project root、os/shell/date + 工作纪律（cwd 锚定、截断重试、commit 自证）。`build_completion_request` 对主 Thread 调用 `system_prompt_fn`（子 agent 不受影响，保留其 `agent_def.rs` 加载的 *.md system prompt）。
- `thread.rs` — `Thread` 状态机（gpui `Entity<Thread>` + `EventEmitter<ThreadEvent>`）。`run_turn` 在 gpui executor 上 spawn 一个 task，task 内循环：`build_completion_request` → `model.stream_completion` → 逐事件 `handle_completion_event` → 收集 `pending_tool_uses` → 流结束后按「免审批并行 / 需审批串行」分区执行（gpui `Task` 非 Send，故并发 spawn + 顺序 await）→ 逐 tool 审批→执行→追加 ToolResult 消息→回到循环。无 tool_use 时 `EndTurn` 退出。子 agent 字段：`system`（子 system prompt，主 Thread=None）、`depth`（主=0，子=父+1）、`max_turns`/`turn_count`/`cap_summary_injected`（截断时注入一轮总结而非硬停）、`pending_authorizations`（id→oneshot，多槽）、`pending_child_auth`（复合 id→`ChildAuthRoute`，授权冒泡路由）。子 agent 对话不单独存内存 map——直接作为 JSON envelope 写进父 ToolResult.content（见「快照与持久化」）。`new_subagent` 构造子 Thread，`tools_fn` 闭包解决「AgentTool 需子 WeakEntity 但子 Thread 还没建」的先有鸡先有蛋问题。

- `language_model.rs` — `LanguageModel` trait（`stream_completion` 返回 `BoxFuture` 产出 `BoxStream<Result<LanguageModelCompletionEvent>>`）。通用类型：`Role`、`MessageContent`、`LanguageModelRequest`、`LanguageModelToolUse`、`LanguageModelToolResult` 等。

- `message.rs` — `Message`（role + `Vec<MessageContent>`，`Serialize/Deserialize`），Thread 持有 `Vec<Message>` 作为规范状态。

- `paths.rs` — 统一的配置目录 helper：`cx_config_dir()`（`$HOME/.config/cx`）/ `manox_config_dir()`（`$HOME/.config/cx/manox`）/ `agents_dir()`（`…/manox/agents`）。HOME 缺失时 warn 并回退到 CWD。

- `agent_def.rs` — 子 agent 定义加载层。从 `agents_dir()/*.md` 读取 frontmatter（`name`/`description`/`tools`/`disallowed_tools`/`model`/`max_turns`/`allow_nesting`）+ markdown 正文（system prompt），`AgentDefinitionRegistry`（`OnceLock` 全局，`init` 时加载，缺文件/解析失败 warn 跳过）。`tools`/`disallowed_tools` 不影响 `agent` 工具本身（仅 `allow_nesting` + 深度上限控制嵌套）。

- `provider/` — LLM provider 集成：
  - `config.rs` — 解析 `~/.config/cx/cx.providers.config.yaml`，产出 `ResolvedModel`（provider + endpoint + wire_api + auth 完全解析）。支持 `WireApi::Anthropic` / `Responses` / `Completions`。
  - `registry.rs` — `ProviderRegistry`（`OnceLock` 全局），启动时加载 config，按 wire_api 构造对应 `LanguageModel` 实现。
  - `anthropic.rs` — Anthropic Messages API 流式客户端（SSE 解析 + `AnthropicEventMapper`）。
  - `completions.rs` — OpenAI Chat Completions wire（仅文本流式，不映射工具/thinking）。
  - `responses.rs` — OpenAI Responses wire（仅文本流式）。
  - `sse.rs` — 通用 SSE 行解析（`data:` 前缀剥离）。
  - `api_key.rs` — 支持 `keychain:SERVICE` / `env:VAR` / `literal:...` / `$(shell ...)` 四种源。

- `tool.rs` + `tool/permission.rs` — `AgentTool` trait + `ToolRegistry` + `PermissionCache`（会话级 always-allow），`PermissionDecision::AllowOnce | AlwaysAllow | Deny`，`ToolAuthorizationResponse`（审批载荷：`Decision(PermissionDecision)` 或 `AskUserQuestion { answers, response }`，后者由 thread 短路为 ToolResult，不经 `run` 执行）。

- `tools/` — 10 个内置工具：`read_file`、`write_file`、`edit_file`、`list_directory`、`bash`、`grep`、`glob`、`ask_user`（`AskUserQuestion`，向用户提多选澄清问题）、`agent`（spawn 子 agent，见「多 agent 系统」）、`self_info`（查看运行时身份：thread id、cwd、project、os、shell、date）。写操作（write_file/edit_file）、bash 与 `AskUserQuestion` 需审批；`agent`/`self_info` 本身不审批。bash 截断阈值 64KB（累计 dropped 总量，前置标注 + 引导收窄命令）；edit_file/read_file 工具描述使用具体例子 `[<abs-path>#<tag>]` 避免裸占位符。

- `runtime.rs` — 全局 tokio runtime（`OnceLock<Handle>`），`init` 时 build 并 forget，`handle()` 取全局 Handle。供 provider 在 gpui executor 上 spawn tokio 任务跑 HTTP 流。

- `db.rs` — `ThreadsDatabase`（SQLite `threads` 表，路径 `$HOME/.config/cx/manox/threads.db`），`Mutex<Connection>` 同步操作。

- `thread_store.rs` — `ThreadStore` 进程全局 Entity（`OnceLock`），管理 Thread 摘要列表，提供 `save_thread` 异步落盘 + refresh。
- `hashline/` — 行锚定补丁系统 + 快照恢复：
  - `mod.rs` — 全局 `SnapshotStore`（`OnceLock<Mutex<>>`），`init()` 在 `agent::init` 时调用。
  - `hash.rs` — `compute_tag` 计算 4-hex 内容哈希（规范化文本的 CRC32）。
  - `snapshot.rs` — `SnapshotStore` 管理 path → `Snapshot`（raw bytes + tag + line_count），`read_file` 记录前调用 `store_snapshot`，`edit_file` 用 `get_snapshot` 验证 tag。
  - `parser.rs` — 补丁解析：支持 `SWAP N.=M`、`DEL N.=M`、`INS.PRE/POST/HEAD/TAIL N`、`SWAP.BLK N` 等操作，产出 `FilePatch` → `Vec<Op>`。
  - `apply.rs` — 补丁应用：`apply` 对 ORIGINAL 行号逐 op 后向应用；tag 过期时 `try_recover` 做 3-way merge（基于内容范围定位快照片段到当前文本）。
  - `block.rs` — 行范围块结构（`BlockError`）。
  - `recovery.rs` — `try_recover` / `try_recover_with_snapshot` 恢复逻辑。
  - 工具链：`read_file` 输出 `[PATH#TAG]` 头部 + `N:TEXT` 带行号内容；`edit_file` 输入 hashline patch 格式，复用 TAG 验证后应用。

**Thread 审批流：** 工具需审批时，`run_tool` 发 `ThreadEvent::ToolCallAuthorization` 携带 `oneshot::Sender`，UI 弹窗后调用 `Thread::respond_authorization` 回传 `ToolAuthorizationResponse`（普通工具为 `Decision(PermissionDecision)`，`AskUserQuestion` 为 `AskUserQuestion { answers, response }`，thread 据此短路生成 ToolResult 而不执行 `run`），task 在 gpui executor 上 `await` oneshot receiver。

### agent-ui crate（`crates/agent-ui/`）

- `workspace.rs` — `Workspace` 顶层视图，持有 `Entity<Thread>` + `Entity<Sidebar>` + `ConversationState`，`cx.subscribe` 处理 `ThreadEvent`（文本/思考/工具增量交给 `ConversationState`，`ToolCallAuthorization` 弹审批 overlay，`Stop` 终态触发 `save_thread` 落盘）。

- `conversation.rs` — `ConversationState`，从 `ThreadEvent` 增量构建扁平 `ConvItem` 列表（`User | Assistant | Reasoning | ToolCall | Error`）。`rebuild_from_messages` 从 Thread 规范消息列表重建视图（加载历史时用）。

- `views/sidebar.rs` — `Sidebar` Entity，订阅 `ThreadStore`，列出历史 Threads，发 `SidebarEvent`（NewThread / OpenThread / DeleteThread）。

- `views/message.rs` — 单条消息渲染：User 块内右对齐卡片、Assistant Markdown + 复制按钮、Reasoning 可折叠、ToolCall 卡片（状态图标 + 等宽输出）、Error 卡片。

- `views/mod.rs` — `centered()` 辅助函数（全宽居中限宽 760px）。

### manox crate（`crates/manox/`）

- `main.rs` — 薄 bin：初始化 tracing → `gpui_component::init` → `agent::init`（注册 tokio runtime + ProviderRegistry + ThreadStore）→ 创建窗口 1100×760 → `agent_ui::Workspace` + `Root`。绑定 `cmd-q` / `alt-f4` 退出、`cmd-ctrl-f`（mac）/ `f11`（其他）切换全屏。

## 关键设计模式

### tokio ↔ gpui 桥接

`agent::runtime` 提供全局 tokio Handle。Provider 的 `stream_completion` 在 gpui executor 上 spawn tokio 任务跑 reqwest 流式 HTTP，通过 `async_channel` 把事件回传 gpui 侧 `BoxStream`（async_channel 执行器无关，可在 gpui executor 上 poll）。

### gpui Entity + EventEmitter 模式

所有有状态组件都是 `Entity<T>` + `EventEmitter<E>`：
- `Thread` emits `ThreadEvent`（UI 订阅增量渲染）
- `ThreadStore` emits `ThreadStoreEvent::SummariesUpdated`（Sidebar 订阅刷新）
- `Sidebar` emits `SidebarEvent`（Workspace 订阅响应操作）

### 全局单例（OnceLock）

- `runtime::handle()` — tokio Handle
- `provider::registry::global()` — ProviderRegistry
- `thread_store::global()` — ThreadStore Entity

### Tool 执行模式

每个工具实现 `AgentTool` trait，`run` 返回 `Task<Result<String, String>>`（`Ok` 为正常输出，`Err` 仍回传模型）。FS 工具经 `cx.background_spawn`，子进程工具（bash/grep）经 `bridge_tokio`（`async_channel` 桥接 tokio → gpui）。

### 多 agent（subagents）系统

主 agent 通过 `agent` 工具（`tools/agent.rs`）spawn 独立 context、受限工具集、独立 system prompt 的子 agent，把其最终回复作为 tool result 回传。对标 Claude Code 的 `Agent` 工具 / Codex 的 `spawn_agent`。

- **定义文件**：`~/.config/cx/manox/agents/*.md`，frontmatter（`name`/`description`/`tools`/`disallowed_tools`/`model`/`max_turns`/`allow_nesting`）+ 正文（system prompt）。`AgentDefinitionRegistry` 启动时加载（`OnceLock` 全局），缺文件/解析失败 warn 跳过。
- **子 Thread 构造**：`Thread::new_subagent` 建独立 `Entity<Thread>`，持 `system`/`depth`/`max_turns`/独立 `PermissionCache`/受限 `ToolRegistry`。`tools_fn` 闭包在 `cx.entity().downgrade()` 可用后构造 registry，解决 AgentTool 需子 WeakEntity 的循环依赖。
- **并行执行**：`run_turn_loop` 把 `pending_tool_uses` 按 `requires_approval()` 分区——免审批工具并行 spawn（gpui `Task` 非 Send，故「并发 spawn + 顺序 await」等价 join_all），需审批工具串行（避免单槽 overlay 覆盖）。cancel 共享同一 `CancellationToken`（clone 广播）。
- **授权冒泡**：子 agent 内部工具需审批时，子 Thread emit `ToolCallAuthorization`，`agent` 工具的订阅把它以**复合 id** `<parent_tool_use_id>::<child_auth_id>` 重新 emit 到父 Thread。`Thread::respond_authorization` 三分支：命中本 Thread `pending_authorizations`→oneshot send；命中 `pending_child_auth`（复合 id）→upgrade child 转发；都不命中→静默（可能已 cancel）。UI overlay 透明处理复合 id，`pending_auths: Vec` 多槽避免两个并行子 agent 同时冒泡时互相覆盖。
- **权限继承**：子 `PermissionCache` 用 `PermissionCache::from_snapshot(parent.permission_snapshot())` 预填父的 always-allow 快照，子不再因父已授权的工具重复弹窗；子新授权不回写父（路由回子 Thread）。
- **深度/嵌套限制**：`Thread.depth` 主=0 子=父+1，`MAX_DEPTH=5`。`build_child_registry` 仅在 `allow_nesting && child_depth<MAX_DEPTH` 时注册 `agent` 工具，`run_streaming` 开头 `depth+1>MAX_DEPTH` 直接 Err。双重保险。
- **工具集范围（MCP 不进子 agent）**：`build_child_registry` 基于 `base_tools` 构造（按 frontmatter `tools`/`disallowed_tools` 过滤），`default_registry` 才在 `base_tools` 之外追加 MCP 工具。故 MCP 工具仅主 agent 可用——子 agent 是受限 context，首版不继承全局 MCP 能力；若 frontmatter `tools` 列了 `mcp_*` 名也会因不在 `base_tools` 而被 `is_tool_allowed` 拒绝。
- **max_turns 截断**：达 `max_turns` 时注入一轮总结 user 消息（`cap_summary_injected` 防二次循环），让子 agent 产出连贯最终回复而非硬停；第二轮再触顶才 `Stop(EndTurn)`。
- **完成信号**：`setup_child` 订阅只对真终态（`Stop(EndTurn|MaxTokens|Refusal)` + `Error`）发 `done_tx`；`Stop(ToolUse)` 是非终端中间态（子下一轮继续跑工具），忽略之，否则会把子第一轮未跑工具的文本当成最终结果回传父。
- **快照与持久化**：子结束后，`agent` 工具把 JSON envelope `{"final":..., "messages":[...]}` 作为 ToolResult.content 写入规范 `Thread::messages`——这是子对话的**唯一来源**（不另存内存 map，长会话无泄漏）。UI live 路径从 `ThreadEvent::ToolResult` 的 `output` 用 `agent_sub_messages` 解析喂展开面板；reload 路径从 `rebuild_from_messages` 同样解析 envelope 还原 `sub_messages`。**上下文隔离**：`build_completion_request` 在映射到模型请求时用 `model_facing_content` 把 `agent` 的 ToolResult envelope 剥成只 `final` 文本——规范消息保留完整 envelope（持久化+UI 用），但父 LLM 只看到 `final`，子 agent 的中间工具调用/结果/reasoning 不泄漏进父上下文。
- **UI**：`ConvItem::AgentTask` 渲染子 agent 卡片（标题=subagent_type+title、状态图标、chevron 展开/折叠）。折叠态显示 `sub_text` live tail；展开态用 `ConversationState::rebuild_from_messages(&sub_messages)` 得临时 state 再递归 `render_item`（递归深度由 `MAX_DEPTH` 数据侧限）。`views/message.rs` 的 `render_agent_task`。

### tokio ↔ gpui 桥接（子 agent 侧）

子 agent 的 `run_turn` 跑在父 turn 的 gpui task 内（非新 executor）；子内部工具经与主 Thread 相同的 `runtime::handle()` tokio 桥接。`agent` 工具 `run_streaming` 的 `cx.spawn` task `await` `done_rx`（`async_channel`，子终态触发），期间 `sink.try_emit` 把子 `AgentText`/`AgentThinking` 流式喂给父 tool 卡片。

## GPUI 依赖版本锁定

- GPUI 栈走 git 仓库地址（规则允许，crates.io 无 gpui-component）：`gpui` / `gpui_platform` pin 到 zed rev `1d217ee39d381ac101b7cf49d3d22451ac1093fe`；`gpui-component` / `gpui-component-assets` pin 到 longbridge rev `a9a7341c35b62f27ff512371c62419342264710c`（upstream main HEAD，2026-07-02）
- gpui-component 锁定 zed rev `1d217ee`，三者（gpui / gpui_platform / gpui-component）必须一致，单一 gpui 版本
- `gpui-rich-text` 是 manox first-party crate（`crates/rich_text`，workspace 成员）：官方 gpui-component 仓库无此 crate，作者本人代码并入自维护
- `ropey`、`sum-tree`（`zed-sum-tree`）随 rich_text 引入，版本与 gpui-component main 对齐
- `psm` patch 到 stacker master 分支（对齐 gpui-component）
- 所有 gpui 相关依赖在 debug 下 opt-level=3，否则渲染极慢

## 运行时配置

- LLM 配置：`~/.config/cx/cx.providers.config.yaml`（格式见 `provider/config.rs` 的 `CxConfig`）
- SQLite 数据库：`~/.config/cx/manox/threads.db`
- 子 agent 定义：`~/.config/cx/manox/agents/*.md`（frontmatter + system prompt，见 `agent_def.rs`）
- MCP 配置：`~/.config/cx/manox/mcp.toml`（`[mcp_servers.<name>]` 表，stdio `command`/`args`/`env`/`cwd` 或 streamable HTTP `url`/`headers`，见 `mcp/config.rs`。纯文件配置，不接入 UI）
- API key 源：macOS Keychain（`keychain:SERVICE`）、环境变量（`env:VAR`）、字面量（`literal:...`）、shell 命令（`$(shell ...)`）

## 项目规则

- **技术选型喜新厌旧**：能选择最新稳定版就选最新稳定版。依赖、工具链、API 都优先用最新的 stable release。
- **禁止 vendor / submodule 依赖**：所有依赖通过包管理器（Cargo）声明，不允许 vendor 目录或 git submodule 引入第三方代码。
- **crate 依赖只认 crate 索引或 git 仓库地址**：引用外部 crate 时，依赖声明只能是 crates.io 版本（crate 索引）或 `git = "..."` 仓库地址，禁止用 `path = "..."` 指向开发者本机文件系统路径（CI 无法复现、不可移植）。workspace 内部成员间的 `path` 引用除外（属于同一仓库，可移植）。
- **只允许单二进制、单进程交付**：最终产物是一个二进制文件，运行时只有一个进程。不允许拆分多个独立可执行文件或需要多进程协作。
- **PR 提交后与 remora 达成一致**：先提交 PR，再运行 `/remora:adversarial-review [prompt]`，与 remora 多轮交锋直到双方达成一致后再合并。
- **禁止抄袭第三方 crate 代码**：若想引入第三方 crate 的特性，应规范引入该 crate 作为依赖。对于不便规范引入的（过重、未暴露相关接口、archived 等），可以参考其架构思想、设计思路、实现方法，但禁止抄袭代码（复制粘贴后修改）。
- **注释规范**：注释一律用英文，面向终态（描述代码维持的不变量/意图）而非过程流水账，非必要不注释。详见 `~/.claude/rules/code-comments.md`。

