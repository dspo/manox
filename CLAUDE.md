# CLAUDE.md

Guidance for Claude Code working in this repo.

## 项目概述

manox 是一个进程内 native agent 工作台，按 Zed 的 agent / agent_ui / zed 三层架构复刻。基于 GPUI（Zed 的 GPU 加速 UI 框架）+ gpui-component（longbridge 组件库），LLM 通过 `~/.config/cx/cx.providers.config.yaml` 直连 provider。**单二进制、单进程**交付。

## 构建与开发命令

```bash
cargo build                          # debug 下 gpui 默认 opt-level=3，否则渲染极慢
cargo run
cargo test                           # live 测试用 MANOX_RUN_LIVE=1 env 门控，默认安全（早退、不发真实 API）
MANOX_RUN_LIVE=1 cargo test          # 真实 API 测试（需 macOS Keychain 或 env 配 key）
cargo test -p agent -- test_name     # 单 crate / 单测试
cargo clippy --all-targets
cargo fmt --all
```

## 工具链

Rust **1.95.0**（`rust-toolchain.toml`），edition **2024**（所有 crate），需 `clippy`/`rustfmt`/`rust-src`。

## 架构总览

```
crates/
├── agent/        # 核心逻辑：Thread 状态机 + LanguageModel + tools + SQLite + i18n + plugin/command/skill/hook 加载
│                  不依赖 gpui-component
├── agent-ui/     # UI 层：Workspace + ConversationState + Sidebar + 消息渲染 + slash command + dispatch
│                  依赖 gpui-component + gpui-component-assets
└── manox/        # 薄 bin：窗口 + 主题 + tracing + 系统菜单 + init 组装
```

### agent crate（`crates/agent/src/`）

- `thread.rs` — `Thread` 状态机（`Entity<Thread>` + `EventEmitter<ThreadEvent>`）。`run_turn` 在 gpui executor 上 spawn task，循环：`build_completion_request` → `stream_completion` → 事件分发 → 收集 `pending_tool_uses` → 按「免审批并行 / 需审批串行」分区执行 → 审批→执行→追加 ToolResult→回循环。无 tool_use 时 `EndTurn` 退出。子 agent 字段：`system`/`depth`/`max_turns`/`cap_summary_injected`/`pending_authorizations`/`pending_child_auth`（授权冒泡路由）。子对话作为 JSON envelope 写进父 ToolResult.content（持久化+UI 用），`build_completion_request` 剥成只 `final` 文本喂模型（上下文隔离）。
- `system_prompt.rs` + `system_prompt.md` — 主 agent system prompt 构建层。`build_main_system_prompt` 注入运行时身份（thread id/cwd/project/os/shell/date）+ 语言指令行（`language_directive`）。子 agent 不走此路径（保留各自 `agents/*.md` 的 system prompt），但 `thread.rs` 在子 `system` 后追加同一语言指令。
- `language_model.rs` — `LanguageModel` trait（`stream_completion` 返回 `BoxFuture<BoxStream>`）+ 通用类型（`Role`/`MessageContent`/`LanguageModelRequest`/`LanguageModelToolUse`/`LanguageModelToolResult`）。
- `message.rs` — `Message`（role + `Vec<MessageContent>`，Serialize/Deserialize）。
- `provider/` — `config.rs` 解析 `cx.providers.config.yaml` 产出 `ResolvedModel`（`WireApi::Anthropic`/`Responses`/`Completions`）；`registry.rs`（`OnceLock` 全局）；`anthropic.rs`（SSE 流式）/`completions.rs`/`responses.rs`（文本流式）/`sse.rs`；`api_key.rs`（`keychain:`/`env:`/`literal:`/`$(shell ...)` 四源）。
- `tool.rs` + `tool/permission.rs` + `tool/plan_mode.rs` — `AgentTool` trait + `ToolRegistry` + `PermissionCache`（会话级 always-allow）+ `exit_plan_mode` 工具（plan mode 下由 `Thread::run_tool_inner` 拦截做审批握手，不经 registry）。
- `tools/` — 12 内置工具：`read_file`/`write_file`/`edit_file`/`list_directory`/`grep`/`glob`/`bash`/`ask_user`/`agent`/`self_info`/`skill` + `exit_plan_mode`。写操作/bash/`AskUserQuestion` 需审批；`agent`/`self_info`/`skill`/`exit_plan_mode` 不审批。bash 截断 64KB；FS 工具用 hashline 行锚定补丁。`base_tools()` 返受限集（子 agent 基线，不含 MCP），`default_registry()` 在其外追加 MCP 工具（仅主 agent）。
- `hashline/` — 行锚定补丁系统：`read_file` 输出 `[PATH#TAG]` + `N:TEXT`，`edit_file` 输入 `SWAP/DEL/INS` patch，tag 过期时 `try_recover` 做 3-way merge。`SnapshotStore`（`OnceLock<Mutex>`）按 path 记快照。
- `mcp/` — MCP 客户端（stdio + streamable HTTP），`config.rs` 解析 `mcp.toml`，`tool.rs` 把 MCP 工具适配为 `AgentTool`。仅主 agent 可用，不进子 agent。
- `agent_def.rs` / `skill.rs` / `command.rs` / `hook.rs` — 子 agent / skill / slash command / hook 定义加载层，frontmatter + markdown 正文，各自由 `OnceLock` 全局 registry 持有，`init` 时加载。slash command 的 `allowed-tools` 把 Claude Code 工具 spec（`Bash(node:*)`）解析成 manox 工具 id 做白名单。
- `plugin.rs` — Claude Code marketplace 生态：`add_marketplace` clone 仓库到 `marketplace_cache_dir`（shell out `git`，禁止 git2），`install` 拷插件树到 `plugins_dir`。skill/command/agent/hook loader 扫 `plugins_dir`，按 `plugin:name` 命名空间注册。
- `i18n.rs` — 见下「i18n」。
- `paths.rs` — 配置目录 helper（`cx_config_dir`/`manox_config_dir`/`agents_dir`/`skills_dir`/`commands_dir`/`plugins_dir`/`marketplace_cache_dir`/`settings_file` 等）。
- `runtime.rs` — 全局 tokio runtime（`OnceLock<Handle>`），provider 在 gpui executor 上 spawn tokio 跑 HTTP 流，经 `async_channel` 回传。
- `db.rs` — `ThreadsDatabase`（SQLite `threads` 表，`Mutex<Connection>`）。
- `thread_store.rs` — `ThreadStore` 进程全局 Entity，管 Thread 摘要列表 + `save_thread` 异步落盘。
- `model_alias.rs` — `sonnet`/`opus`/`haiku`/`gpt-5`/`o3` 别名 → 实际模型解析。
- `sandbox.rs` — macOS seatbelt 沙盒策略（bash 命令包装，禁网络/禁 `.git` 写）。
- `settings.rs` — 读 `settings.toml`（目前仅 `language` 字段）。

**Thread 审批流**：工具需审批时 `run_tool` 发 `ThreadEvent::ToolCallAuthorization` 携 `oneshot::Sender`，UI 弹 overlay 后调 `Thread::respond_authorization` 回传 `ToolAuthorizationResponse`（普通工具 `Decision(PermissionDecision)`，`AskUserQuestion` 为 `AskUserQuestion { answers, response }` 短路成 ToolResult 不执行 `run`）。子 agent 审批以复合 id `<parent_tool_use_id>::<child_auth_id>` 冒泡到父 Thread。

### agent-ui crate（`crates/agent-ui/src/`）

- `workspace.rs` — `Workspace` 顶层视图，持 `Entity<Thread>` + `Entity<Sidebar>` + `ConversationState`，订阅 `ThreadEvent`（文本/思考/工具增量→`ConversationState`；`ToolCallAuthorization`/`PlanProposed`/`AskUser` 弹 overlay；`Stop` 终态触发 `save_thread`）。
- `conversation.rs` — `ConversationState`，从 `ThreadEvent` 增量构建扁平 `ConvItem` 列表（`User`/`Assistant`/`Reasoning`/`ToolCall`/`AgentTask`/`Error`）。`rebuild_from_messages` 从规范消息列表重建（加载历史/sub-agent envelope 还原用）。
- `dispatch.rs` — App-level action 到 `Workspace`/`WindowHandle` 的桥（macOS 系统菜单回调里 `cx.active_window()` 不可靠，故全局 stash 句柄）。
- `slash_command.rs` — `/name [args]` 解析 + `SlashCommandRegistry`（`OnceLock`，`init` 时注册内置 `/plan`、mock `/yolo` + markdown 命令）。
- `views/sidebar.rs` — `Sidebar` Entity，订阅 `ThreadStore`，列历史 Threads，发 `SidebarEvent`。
- `views/message.rs` — 单条消息渲染（Markdown + 复制按钮 + Reasoning 折叠 + ToolCall/AgentTask 卡片，子 agent 卡片递归 `render_item`）。
- `views/settings.rs` — Settings 同窗覆盖层。
- `views/composer_menu.rs` — `+`/`⁄` 弹出菜单 + 待发送附件。

### manox crate（`crates/manox/src/`）

- `main.rs` — 薄 bin：tracing → `gpui_component::init` → `agent::init` → `agent_ui::slash_command::init` → 绑键 → 系统菜单 → 创建窗口 1100×760 → `Workspace` + `Root`。系统菜单标签经 `i18n::t` 本地化。

## 关键设计模式

- **tokio ↔ gpui 桥接**：`runtime::handle()` 提供全局 tokio Handle。Provider 在 gpui executor 上 spawn tokio 跑 reqwest 流式，经 `async_channel` 把事件回传 gpui 侧 `BoxStream`（async_channel 执行器无关）。
- **gpui Entity + EventEmitter**：`Thread` emits `ThreadEvent`，`ThreadStore` emits `SummariesUpdated`，`Sidebar` emits `SidebarEvent`——UI 订阅增量渲染。
- **全局单例（OnceLock）**：`runtime::handle()` / `provider::registry::global()` / `thread_store::global()` / `mcp::registry::global()` / `agent_def`/`skill`/`command`/`hook` 各 registry / `i18n::current()`。
- **多 agent（subagents）系统**：`agent` 工具 spawn 独立 context、受限工具集、独立 system prompt 的子 Thread，最终回复作为 tool result 回传。深度上限 `MAX_DEPTH=5`，`allow_nesting` + 深度双重保险。授权冒泡经复合 id，权限快照继承（子不再因父已授权工具重复弹窗）。MCP 不进子 agent。
- **Tool 执行**：`run` 返 `Task<Result<String,String>>`（`Ok` 正常输出，`Err` 仍回传模型）。FS 工具经 `cx.background_spawn`，子进程工具经 tokio 桥。

## i18n（重要：开发时勿忘）

manox 区分**模型面向**与**用户面向**两条字符串边界，二者不可混淆：

1. **模型面向一律英文，绝不本地化**：`system_prompt.md`/`system_prompt.rs` 散文、`max_turns_summary_prompt`、所有工具 `description()`、工具 `run` 返回的 Err 字符串、`thread.rs` 里 LLM 能读到的消息（`"unknown tool"`、`"User responded:"`、hashline patch 错误等）。这些**永远不经过 i18n**，无论 UI 语言为何。新增任何 LLM 可见的字符串一律英文。
2. **仅 UI chrome 本地化**：按钮、标签、状态徽章、overlay 标题、输入占位符、侧栏、设置面板、时间相对格式、系统菜单。这些经 `agent::i18n::t("key")` / `t_str(key, &[("arg", v)])` / `t_count(key, n)`。
3. **fluent 资源**在 `crates/agent/locales/{en,zh-CN}.ftl`，`include_str!` 编译期嵌入。**新增 UI 字符串 = 在两个 `.ftl` 各加一个键 + 调用处换 `t("key")`**，缺一不可（缺失键在开发期会渲染成 key 本身以暴露漏译）。
4. **fluent id 用 `-` 分隔**（`.` 非法）。复数用 ICU 选择 `{ $count -> [one] ... *[other] ... }`（zh-CN 无单复数）。**`.ftl` 值里的 `\n` 是字面量不是换行**——换行在 Rust 侧 `format!("{}\n", t(key))` 拼接。
5. **`FluentBundle` 是 `!Send`**（intl-memoizer 持 `RefCell`）→ 用 `thread_local!` 而非 `OnceLock<Mutex>`；locale 选择是进程级 `OnceLock<Language>`。`bundle.set_use_isolating(false)` 关 bidi 隔离符（否则 U+2068/2069 泄漏进字符串）。
6. **语言来自 `~/.config/cx/manox/settings.toml` 的 `language` 字段**（`"en"`/`"zh-CN"`，容错 `zh`/`zh-Hans`/`en-US`），`agent::init` 时读一次，无运行时切换。模型面向语言经 `language_directive()` 注入 system prompt 一行（`Unless the user specifies otherwise, write your user-facing responses in {English/Simplified Chinese}.`），prompt 散文仍英文。

## 运行时配置（`~/.config/cx/`）

- LLM：`cx.providers.config.yaml`（格式见 `provider/config.rs`）
- SQLite：`cx/manox/threads.db`
- 设置：`cx/manox/settings.toml`（`language` 字段）
- 子 agent：`cx/manox/agents/*.md`（frontmatter `name`/`description`/`tools`/`disallowed_tools`/`model`/`max_turns`/`allow_nesting` + 正文 system prompt）
- MCP：`cx/manox/mcp.toml`（`[mcp_servers.<name>]`，stdio `command`/`args`/`env`/`cwd` 或 HTTP `url`/`headers`）
- 插件：`cx/manox/plugins/`（marketplace clone 在 `cx/manox/marketplace-cache/`）
- API key 源：macOS Keychain（`keychain:SERVICE`）/ env（`env:VAR`）/ 字面量（`literal:...`）/ shell（`$(shell ...)`）

## GPUI 依赖版本锁定

- GPUI 栈走 git 仓库地址（crates.io 无 gpui-component）：`gpui`/`gpui_platform` pin zed rev `1d217ee39d381ac101b7cf49d3d22451ac1093fe`；`gpui-component`/`gpui-component-assets` pin longbridge rev `a9a7341c35b62f27ff512371c62419342264710c`。三者必须一致，单一 gpui 版本。
- `gpui-rich-text`（`crates/rich_text`）是 manox first-party crate（官方 gpui-component 仓库无此 crate）。`ropey`/`sum-tree` 随之引入，版本与 gpui-component main 对齐。`psm` patch 到 stacker master。
- gpui 相关依赖在 debug 下 opt-level=3，否则渲染极慢。

## 项目规则

- **技术选型喜新厌旧**：能选最新 stable 就选最新 stable（依赖、工具链、API）。
- **禁止 vendor / submodule**：所有依赖经 Cargo 声明，不允许 vendor 目录或 git submodule 引入第三方代码。
- **crate 依赖只认 crate 索引或 git 仓库地址**：外部 crate 依赖只能是 crates.io 版本或 `git = "..."`，禁止 `path = "..."` 指向本机路径（CI 不可复现）。workspace 内部成员间 `path` 例外。
- **只允许单二进制、单进程交付**：最终产物一个二进制，运行时一个进程。
- **PR 提交后与 remora 达成一致**：先提交 PR，再运行 `/remora:adversarial-review [prompt]`，多轮交锋达成一致后再合并。
- **禁止抄袭第三方 crate 代码**：不便规范引入的可参考架构思想，但禁止复制粘贴后修改。`git2` 即因此被禁（plugin marketplace shell out 系统 `git`）。
- **注释一律英文，面向终态（描述不变量/意图）而非过程流水账，非必要不注释**。详见 `~/.claude/rules/code-comments.md`。
