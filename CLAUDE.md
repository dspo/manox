# CLAUDE.md

Guidance for Claude Code working in this repo.

## 项目概述

manox 是进程内 native agent 工作台，按 Zed 的 agent / agent_ui / zed 三层架构复刻。基于 GPUI（Zed 的 GPU 加速 UI 框架）+ gpui-component（longbridge 组件库），LLM 通过 `~/.config/cx/cx.providers.config.yaml` 直连 provider。**单二进制、单进程**。

## 构建与开发命令

```bash
cargo build                          # debug 下 gpui 默认 opt-level=3，否则渲染极慢
cargo run
cargo test                           # live 测试用 MANOX_RUN_LIVE=1 env 门控，默认安全
MANOX_RUN_LIVE=1 cargo test          # 真实 API 测试（需 macOS Keychain 或 env 配 key）
cargo test -p agent -- test_name     # 单 crate / 单测试
cargo clippy --all-targets
cargo fmt --all
```

## 工具链 & Skills

Rust **1.95.0**（`rust-toolchain.toml`），edition **2024**，需 `clippy`/`rustfmt`/`rust-src`。

GPUI/UI 开发时使用 `.claude/skills/` 下的 skill：
- `gpui` — GPUI 框架（Entity/Render/actions/keybindings/async/layout）
- `gpui-component` — gpui-component 组件库（Button/Input/List/Sidebar 等）
- `gpui-component-dev` — 为 gpui-component 贡献新组件

## 架构总览

```
crates/
├── agent/        # 核心逻辑：Thread + LanguageModel + tools + MCP + SQLite + i18n + plugin/agent/skill/command/hook
├── agent-ui/     # UI 层：Workspace + ConversationState + Sidebar + 消息渲染 + slash command + dispatch
├── manox/        # 薄 bin：窗口 + 主题 + tracing + 系统菜单 + --mcp server 模式（debug feature）
├── terminal/     # 终端模拟器（alacritty_terminal + portable-pty）
└── terminal-ui/  # 终端 UI 渲染
```

### agent crate（`crates/agent/src/`）

- `thread.rs` — `Thread` 状态机（`Entity<Thread>` + `EventEmitter<ThreadEvent>`）。`run_turn` 循环：`build_completion_request` → `stream_completion` → 收集 `pending_tool_uses` → 按「免审批并行/需审批串行」分区执行 → 审批→执行→追加 ToolResult→回循环。子 agent 以 JSON envelope 写进父 ToolResult.content，`build_completion_request` 剥成只 `final` 文本喂模型。工具需审批时发 `ThreadEvent::ToolCallAuthorization` 携 `oneshot::Sender`，UI 弹 overlay 后 `respond_authorization` 回传；子 agent 审批以复合 id `<parent_tool_use_id>::<child_auth_id>` 冒泡。
- `system_prompt.rs` + `system_prompt.md` — 主 agent system prompt 构建层。`build_main_system_prompt` 注入运行时身份（thread id/cwd/project/os/shell/date）+ 语言指令行。子 agent 保留各自 `agents/*.md` 的 system prompt，`thread.rs` 追加同一语言指令。
- `language_model.rs` / `message.rs` — `LanguageModel` trait + 通用类型（`Role`/`MessageContent`/`LanguageModelRequest`/`LanguageModelToolUse`/`LanguageModelToolResult`）。
- `provider/` — `config.rs` 解析 `cx.providers.config.yaml`；`anthropic.rs`（SSE 流式）/`completions.rs`/`responses.rs`；`retry.rs`（指数退避）/`anthropic_cache.rs`（prompt caching）；`api_key.rs`（keychain:/env:/literal:/$(shell) 四源）。
- `tools/` — 14 内置工具（`read_file`/`write_file`/`edit_file`/`bash`/`grep`/`glob`/`list_directory`/`agent`/`ask_user`/`self_info`/`skill`/`worktree`/`monitor` + `exit_plan_mode`）。写操作/bash/`AskUserQuestion` 需审批；`base_tools()` 返受限集（子 agent 基线），`default_registry()` 追加 MCP 工具（仅主 agent）。
- `hashline/` — 行锚定补丁系统：`read_file` 输出 `[PATH#TAG]` + `N:TEXT`，`edit_file` 输入 `SWAP/DEL/INS` patch，tag 过期时 `try_recover` 做 3-way merge。`mcp/` — MCP 客户端（stdio + streamable HTTP），仅主 agent。
- `prefix_stability.rs` / `token_meter.rs` / `title_state.rs` — 前缀缓存诊断 / 流式 token 计费 / LLM 生成标题生命周期。`approval.rs` + `approval/prompt.md` — 审批处理；`hook.rs` — 生命周期 hook。
- `agent_def.rs` / `skill.rs` / `command.rs` / `plugin.rs` — 子 agent/skill/slash command/hook/plugin 定义加载层，frontmatter + markdown 正文，`OnceLock` 全局 registry。
- `db/` — SQLite（`threads`/`terminals`/`events`/`token_usage` 表），`Mutex<Connection>`。`i18n.rs` / `paths.rs` / `runtime.rs` / `settings.rs` / `model_alias.rs` / `sandbox.rs` / `path_env.rs` / `frontmatter.rs` — 基础设施。

### agent-ui crate（`crates/agent-ui/src/`）

- `workspace.rs` — 顶层视图，持 `Entity<Thread>` + `Entity<Sidebar>` + `ConversationState`，订阅 `ThreadEvent`。
- `conversation.rs` — `ConversationState`，从 `ThreadEvent` 增量构建扁平 `ConvItem` 列表。`rebuild_from_messages` 从规范消息列表重建。
- `views/` — `sidebar.rs`（历史 Threads）、`message.rs`（Markdown 渲染 + Reasoning 折叠 + ToolCall/AgentTask 卡片）、`composer_menu.rs`（`+`/`⁄` 弹出菜单）、`settings/`、`plugin_manager.rs`、`title_menu.rs`、`outline.rs`。
- `dispatch.rs` / `slash_command.rs` — action 桥接 + `/name [args]` 解析注册。`harness/` — 进程内 debug harness（debug feature）。

### terminal / terminal-ui / manox

- `terminal` — 完整终端模拟器（alacritty_terminal + portable-pty），含 PTY 管理、事件流、终端状态存储。
- `terminal-ui` — 终端 UI 渲染（grid renderer + terminal view + theme）。
- `manox` — 薄 bin：tracing → `gpui_component::init` → `agent::init` → `agent_ui::slash_command::init` → 绑键 → 系统菜单 → 创建窗口 → `Workspace` + `Root`。`--mcp` 启动 MCP stdio server（debug feature）。

### 关键设计模式

- **tokio ↔ gpui 桥接**：`runtime::handle()` 提供全局 tokio Handle。Provider 在 gpui executor 上 spawn tokio 跑 reqwest 流式，经 `async_channel` 回传 gpui 侧 `BoxStream`。
- **gpui Entity + EventEmitter**：`Thread` → `ThreadEvent`，`ThreadStore` → `SummariesUpdated`，`Sidebar` → `SidebarEvent`——UI 订阅增量渲染。
- **全局单例（OnceLock）**：`runtime::handle()` / `provider::registry::global()` / `thread_store::global()` / `mcp::registry::global()` / agent/skill/command/hook 各 registry。
- **多 agent（subagents）**：`agent` 工具 spawn 独立 context、受限工具集、独立 system prompt 的子 Thread。深度上限 `MAX_DEPTH=5`，`allow_nesting` + 深度双重保险。授权冒泡经复合 id，权限快照继承。MCP 不进子 agent。
- **Tool 执行**：`run` 返 `Task<Result<String,String>>`（`Ok` 正常，`Err` 仍回传模型）。FS 工具经 `cx.background_spawn`，子进程工具经 tokio 桥。
- **前缀缓存**：`build_completion_request` 保持跨 turn 请求前缀字节稳定，配合 `cache_control`/`prompt_cache_key` 使 provider 命中缓存。任何对消息组装管线的改动不得破坏前缀稳定性。`prefix_stability.rs` 提供 `AppendOnlyContextManager`。

## i18n（重要：开发时勿忘）

manox 区分**模型面向**与**用户面向**两条字符串边界：

1. **模型面向一律英文，绝不本地化**：`system_prompt.md` 散文、所有工具 `description()`、工具 `run` 返回的 Err 字符串、`thread.rs` 里 LLM 能读到的消息。这些永远不经过 i18n。
2. **仅 UI chrome 本地化**：按钮、标签、状态徽章、overlay 标题、输入占位符、侧栏、设置面板、系统菜单。经 `agent::i18n::t("key")`。
3. **Fluent 资源**在 `crates/agent/locales/{en,zh-CN}.ftl`，`include_str!` 编译期嵌入。**新增 UI 字符串 = 在两个 `.ftl` 各加一个键 + 调用处换 `t("key")`**，缺一不可。语言来自 `~/.config/cx/manox/settings.toml`，`agent::init` 时读一次，无运行时切换。

## 提示词系统

非必要不将提示词硬编码到 `.rs` 中，用 `.md` 文本文件维护，`include_str!` 编译期嵌入：

- `system_prompt.md` — 主 agent system prompt；`agents/*.md` — 子 agent 定义（frontmatter + 正文）
- `approval/prompt.md` — 审批 reviewer 提示词；`skills/<name>/SKILL.md` — 技能定义

短参数化模板（1-2 句）可保留在 `.rs` 中，多段落散文一律用 `.md`。

## 运行时配置（`~/.config/cx/`）

- LLM：`cx.providers.config.yaml`（格式见 `provider/config.rs`）；SQLite：`cx/manox/threads.db`；设置：`cx/manox/settings.toml`
- 子 agent：`cx/manox/agents/*.md`（frontmatter name/description/tools/model/max_turns/allow_nesting + 正文）；MCP：`cx/manox/mcp.toml`（stdio command/args/env/cwd 或 HTTP url/headers）
- 插件：`cx/manox/plugins/`（marketplace cache 在 `cx/manox/marketplace-cache/`）；API key 源：macOS Keychain（`keychain:SERVICE`）/ env（`env:VAR`）/ 字面量（`literal:...`）/ shell（`$(shell ...)`）

## GPUI 依赖版本锁定

- GPUI 栈走 git 仓库地址（crates.io 无 gpui-component）：`gpui`/`gpui_platform` pin zed rev；`gpui-component`/`gpui-component-assets` pin longbridge rev。三者必须一致，单一 gpui 版本。`gpui-rich-text`（`crates/rich_text`）是 manox first-party crate。gpui 相关依赖在 debug 下 opt-level=3，否则渲染极慢。

## 项目规则

- **技术选型喜新厌旧**：能选最新 stable 就选最新 stable（依赖、工具链、API）。
- **禁止 vendor / submodule**：所有依赖经 Cargo 声明，不允许 vendor 目录或 git submodule 引入第三方代码。
- **crate 依赖只认 crate 索引或 git 仓库地址**：外部 crate 依赖只能是 crates.io 版本或 `git = "..."`，禁止 `path = "..."` 指向本机路径（CI 不可复现）。workspace 内部成员间 `path` 例外。
- **只允许单二进制、单进程交付**：最终产物一个二进制，运行时一个进程。
- **PR 提交后与 remora 达成一致**：先提交 PR，再运行 `/remora:adversarial-review [prompt]`，多轮交锋达成一致后再合并。
- **禁止抄袭第三方 crate 代码**：不便规范引入的可参考架构思想，但禁止复制粘贴后修改。`git2` 即因此被禁（plugin marketplace shell out 系统 `git`）。
- **注释一律英文，面向终态（描述不变量/意图）而非过程流水账，非必要不注释**。详见 `~/.claude/rules/code-comments.md`。
- **迭代时不得破坏前缀缓存**：provider 侧前缀缓存是透明优化——命中时零成本，击穿时静默回退。任何对 `build_completion_request` 或消息组装管线的改动，必须保持跨 turn 的请求前缀字节一致；若需重写历史，须先接入 `AppendOnlyContextManager`（`prefix_stability.rs`）或显式禁用该路径的缓存。
- **涉及 GPUI/UI 开发时，先 load skills**：任何与 GPUI 框架或 gpui-component 组件库相关的 UI 任务，应在开始实现前通过 Skill 工具加载 `.claude/skills/gpui` 和 `.claude/skills/gpui-component`（贡献新组件时额外加载 `gpui-component-dev`），确保遵循 GPUI Entity/Render/actions/keybindings/async/layout 惯用法和 gpui-component 组件 API。
- **重构 UI 后，及时修订 UI-MAP.md**：任何对 UI 组件层级、命名、添加/移除/重组组件的变更，都必须在同一 PR 中更新 `UI-MAP.md`，保持组件名、层级关系和源码位置与代码一致。新增组件要在索引和对应章节各加一个 `####` 标题，移除组件要同步删索引条目。
- **零构建告警**：CI 以 `-D warnings` 编译，任何 error 或 warning 都会让 CI 红灯。提交前必须本地跑 `cargo clippy --all-targets -- -D warnings` 全绿，`cargo build` 无 warning。新增 `#[allow(...)]` 视为逃避而非修复，除非该 lint 本身与项目既有设计冲突（如 GPUI 派生宏触发的假阳性），且必须在 `#[allow]` 处用英文注释说明为何该 lint 不适用。`Result` 必须 `let _ =` 或 `?` 处理，禁止裸丢弃；test 模块必须在文件末尾。

## 激进开发纪律

manox 处于开发早期，不维护 v0→v1 升级路径，不背历史负债。

- **不写升级脚本 / migration**：schema 变更靠 `DROP TABLE + CREATE TABLE` 全量重建（`db/mod.rs`），不写 `ALTER TABLE`/多步迁移链。
- **不保留兼容字段 / 不写 fallback 兼容读**：字段失去存在理由时直接删，不要 `#[serde(rename)]`/双写/`Option::unwrap_or(default)` 续命。读不到 key 就报错，不静默回退。
- **不写 `v0`/`legacy_`/`backward_compat` 模块**：任何以向后兼容为名的子模块/helper/trait/wrapper 直接拒。新枚举/新 schema 直接上，原地替换。删代码时同步删测试。
> 当不确定要不要保留兼容层时，问：当前有没有外部用户的数据会因此被破坏？答案是"没有 / 用户可以接受丢"——就按激进方向走。
