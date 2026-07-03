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

- `thread.rs` — `Thread` 状态机（gpui `Entity<Thread>` + `EventEmitter<ThreadEvent>`）。`run_turn` 在 gpui executor 上 spawn 一个 task，task 内循环：`build_completion_request` → `model.stream_completion` → 逐事件 `handle_completion_event` → 收集 `pending_tool_uses` → 流结束后逐 tool 审批→执行→追加 ToolResult 消息→回到循环。无 tool_use 时 `EndTurn` 退出。

- `language_model.rs` — `LanguageModel` trait（`stream_completion` 返回 `BoxFuture` 产出 `BoxStream<Result<LanguageModelCompletionEvent>>`）。通用类型：`Role`、`MessageContent`、`LanguageModelRequest`、`LanguageModelToolUse`、`LanguageModelToolResult` 等。

- `message.rs` — `Message`（role + `Vec<MessageContent>`），Thread 持有 `Vec<Message>` 作为规范状态。

- `provider/` — LLM provider 集成：
  - `config.rs` — 解析 `~/.config/cx/cx.providers.config.yaml`，产出 `ResolvedModel`（provider + endpoint + wire_api + auth 完全解析）。支持 `WireApi::Anthropic` / `Responses` / `Completions`。
  - `registry.rs` — `ProviderRegistry`（`OnceLock` 全局），启动时加载 config，按 wire_api 构造对应 `LanguageModel` 实现。
  - `anthropic.rs` — Anthropic Messages API 流式客户端（SSE 解析 + `AnthropicEventMapper`）。
  - `completions.rs` — OpenAI Chat Completions wire（仅文本流式，不映射工具/thinking）。
  - `responses.rs` — OpenAI Responses wire（仅文本流式）。
  - `sse.rs` — 通用 SSE 行解析（`data:` 前缀剥离）。
  - `api_key.rs` — 支持 `keychain:SERVICE` / `env:VAR` / `literal:...` / `$(shell ...)` 四种源。

- `tool.rs` + `tool/permission.rs` — `AgentTool` trait + `ToolRegistry` + `PermissionCache`（会话级 always-allow），`PermissionDecision::AllowOnce | AlwaysAllow | Deny`。

- `tools/` — 7 个内置工具：`read_file`、`write_file`、`edit_file`、`list_directory`、`bash`、`grep`、`glob`。写操作（write_file/edit_file）和 bash 需审批。bash/grep 经 tokio 子进程 + `async_channel` 桥回 gpui Task。

- `runtime.rs` — 全局 tokio runtime（`OnceLock<Handle>`），`init` 时 build 并 forget，`handle()` 取全局 Handle。供 provider 在 gpui executor 上 spawn tokio 任务跑 HTTP 流。

- `db.rs` — `ThreadsDatabase`（SQLite `threads` 表，路径 `$HOME/.config/cx/manox/threads.db`），`Mutex<Connection>` 同步操作。

- `thread_store.rs` — `ThreadStore` 进程全局 Entity（`OnceLock`），管理 Thread 摘要列表，提供 `save_thread` 异步落盘 + refresh。

**Thread 审批流：** 工具需审批时，`run_tool` 发 `ThreadEvent::ToolCallAuthorization` 携带 `oneshot::Sender`，UI 弹窗后调用 `Thread::respond_authorization` 回传 `PermissionDecision`，task 在 gpui executor 上 `await` oneshot receiver。

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
- API key 源：macOS Keychain（`keychain:SERVICE`）、环境变量（`env:VAR`）、字面量（`literal:...`）、shell 命令（`$(shell ...)`）

## 项目规则

- **技术选型喜新厌旧**：能选择最新稳定版就选最新稳定版。依赖、工具链、API 都优先用最新的 stable release。
- **禁止 vendor / submodule 依赖**：所有依赖通过包管理器（Cargo）声明，不允许 vendor 目录或 git submodule 引入第三方代码。
- **crate 依赖只认 crate 索引或 git 仓库地址**：引用外部 crate 时，依赖声明只能是 crates.io 版本（crate 索引）或 `git = "..."` 仓库地址，禁止用 `path = "..."` 指向开发者本机文件系统路径（CI 无法复现、不可移植）。workspace 内部成员间的 `path` 引用除外（属于同一仓库，可移植）。
- **只允许单二进制、单进程交付**：最终产物是一个二进制文件，运行时只有一个进程。不允许拆分多个独立可执行文件或需要多进程协作。
- **PR 提交后与 remora 达成一致**：先提交 PR，再运行 `/remora:adversarial-review [prompt]`，与 remora 多轮交锋直到双方达成一致后再合并。
- **禁止抄袭第三方 crate 代码**：若想引入第三方 crate 的特性，应规范引入该 crate 作为依赖。对于不便规范引入的（过重、未暴露相关接口、archived 等），可以参考其架构思想、设计思路、实现方法，但禁止抄袭代码（复制粘贴后修改）。
- **注释规范**：注释一律用英文，面向终态（描述代码维持的不变量/意图）而非过程流水账，非必要不注释。详见 `~/.claude/rules/code-comments.md`。