# CLAUDE.md

Guidance for Claude Code working in this repo.

## 项目概述

manox 是进程内 native agent 工作台，三层架构（核心逻辑 `crates/agent` / UI 层 `crates/agent-ui` / 薄 bin `crates/manox`，另有 `terminal` + `terminal-ui`）。基于 GPUI（GPU 加速 UI 框架）+ gpui-component（longbridge 组件库），LLM 通过 `~/.config/cx/cx.providers.config.yaml` 直连 provider。**单二进制、单进程**。逐文件架构靠读代码获得——本文件只承载不可从代码推导的约束。

## 构建与开发命令

```bash
cargo build                          # debug 下 gpui 依赖需 opt-level=3，否则渲染极慢
cargo run
cargo test                           # live 测试用 MANOX_RUN_LIVE=1 env 门控，默认安全
MANOX_RUN_LIVE=1 cargo test          # 真实 API 测试（需 macOS Keychain 或 env 配 key）
cargo test -p agent -- test_name     # 单 crate / 单测试
cargo clippy --all-targets
cargo fmt --all
```

Rust **1.95.0**（`rust-toolchain.toml`），edition **2024**，需 `clippy`/`rustfmt`/`rust-src`。

## 工具链 & Skills

涉及 GPUI/UI 开发时，先通过 Skill 工具加载 `.claude/skills/` 下的 skill：
- `gpui` — GPUI 框架（Entity/Render/actions/keybindings/async/layout）
- `gpui-component` — gpui-component 组件库（Button/Input/List/Sidebar 等）
- `gpui-component-dev` — 为 gpui-component 贡献新组件时额外加载

## i18n（重要：开发时勿忘）

manox 区分**模型面向**与**用户面向**两条字符串边界：

1. **模型面向一律英文，绝不本地化**：`system_prompt.md` 散文、所有工具 `description()`、工具 `run` 返回的 Err 字符串、`thread.rs` 里 LLM 能读到的消息。永远不经 i18n。
2. **仅 UI chrome 本地化**：按钮、标签、状态徽章、overlay 标题、输入占位符、侧栏、设置面板、系统菜单。经 `agent::i18n::t("key")`。
3. **Fluent 资源**在 `crates/agent/locales/{en,zh-CN}.ftl`，`include_str!` 编译期嵌入。**新增 UI 字符串 = 在两个 `.ftl` 各加一个键 + 调用处换 `t("key")`**，缺一不可。语言来自 `~/.config/cx/manox/settings.toml`：UI 语言（`ui_language`）`agent::init` 时读一次，可运行时热切换（`i18n::set_ui_language`，仅 chrome 重新本地化，已产生的内容不回改）；agent 语言（`agent_language`）按 thread 快照、终身不变。

## 提示词系统

非必要不将提示词硬编码到 `.rs` 中，用 `.md` 文本文件维护，`include_str!` 编译期嵌入：`system_prompt.md`（主 agent）、`agents/*.md`（子 agent 定义）、`approval/prompt.md`（审批 reviewer）、`skills/<name>/SKILL.md`（技能）。短参数化模板（1-2 句）可留在 `.rs`，多段落散文一律用 `.md`。

## 运行时配置（`~/.config/cx/`）

- LLM：`cx.providers.config.yaml`（格式见 `provider/config.rs`）；SQLite：`cx/manox/threads.db`；设置：`cx/manox/settings.toml`
- 子 agent：`cx/manox/agents/*.md`（frontmatter name/description/tools/model/max_turns/allow_nesting + 正文）；MCP：`cx/manox/mcp.toml`（stdio 或 HTTP）；插件：`cx/manox/plugins/`
- API key 源：macOS Keychain（`keychain:SERVICE`）/ env（`env:VAR`）/ 字面量（`literal:...`）/ shell（`$(shell ...)`）
- **百炼 anthropic 兼容端点**（`*.aliyuncs.com/apps/anthropic`）：不报 `cache_creation_input_tokens`（恒 0），只报 `cache_read_input_tokens`。故 manox 的 `cache_creation` 记账对该端点恒 0 属预期，非解析/累加/持久化 bug（三链路均正确，记的就是端点报的 0）。`LastBreakpointOnly` 与 `Full` policy 对该端点均有效，由 `provider/anthropic.rs` 的 `MANOX_RUN_LIVE` 门控探针确证。

## GPUI 依赖版本锁定

GPUI 栈走 git 仓库地址（crates.io 无 gpui-component）：`gpui`/`gpui_platform` pin zed rev，`gpui-component`/`gpui-component-assets` pin longbridge rev，**三者必须同一 gpui 版本**。`gpui-rich-text`（`crates/rich_text`）是 manox first-party crate。gpui 相关依赖在 debug 下需 opt-level=3。

## 项目规则

- **技术选型喜新厌旧**：能选最新 stable 就选最新 stable（依赖、工具链、API）。
- **禁止 vendor / submodule**：所有依赖经 Cargo 声明，不允许 vendor 目录或 git submodule。
- **crate 依赖只认 crate 索引或 git 地址**：外部 crate 只能是 crates.io 版本或 `git = "..."`，禁止 `path = "..."` 指向本机路径（CI 不可复现）；workspace 内部成员间 `path` 例外。
- **只允许单二进制、单进程交付**：最终产物一个二进制，运行时一个进程。
- **PR 提交后与 remora 达成一致**：先提交 PR，再运行 `/remora:adversarial-review [prompt]`，多轮交锋达成一致后再合并。
- **禁止抄袭第三方 crate 代码**：可参考架构思想，禁止复制粘贴后修改。`git2` 即因此被禁（plugin marketplace shell out 系统 `git`）。
- **注释一律英文，面向终态**（描述不变量/意图）而非过程流水账，非必要不注释。详见 `~/.claude/rules/code-comments.md`。
- **迭代时不得破坏前缀缓存**：provider 侧前缀缓存是透明优化（命中零成本，击穿静默回退）。任何对 `build_completion_request` 或消息组装管线的改动，必须保持跨 turn 请求前缀字节一致；若需重写历史，须先接入 `AppendOnlyContextManager`（`prefix_stability.rs`）或显式禁用该路径缓存。
- **零构建告警**：CI 以 `-D warnings` 编译。提交前必须本地 `cargo clippy --all-targets -- -D warnings` 全绿、`cargo build` 无 warning。新增 `#[allow(...)]` 视为逃避而非修复，除非该 lint 本身与项目设计冲突（如 GPUI 派生宏假阳性），且必须在 `#[allow]` 处用英文注释说明。`Result` 必须 `let _ =` 或 `?` 处理，禁止裸丢弃；test 模块必须在文件末尾。
- **重构 UI 后及时修订 `UI-MAP.md`**：任何 UI 组件层级、命名、增删重组的变更，必须在同一 PR 更新 `UI-MAP.md`（组件名/层级/源码位置与代码一致，索引与章节各加/删一个标题）。
- **勿以善小而不为**：对正面有效的 review 意见，即便不构成阻塞也应尽量遵从。
- **Plan 应写入实施方式**：制定涉及编码的 Plan 时，将「按 /gitwork:deliver 实施」写入 Plan 正文。

## 激进开发纪律

manox 处于开发早期，不维护 v0→v1 升级路径，不背历史负债。

- **运行时禁止 schema migration**：运行时永不改 DDL、不删除/重建用户机器上的 db。`db/mod.rs` 的 `open()` 仅对全新 db `CREATE TABLE IF NOT EXISTS`，对已有 db 是 no-op。开发中的 schema 迁移，直接手动改开发机 db（`sqlite3` CLI / `ALTER TABLE` / 重建）。schema 不对就该报错报错、该 panic panic。
- **不保留兼容字段 / 不写 fallback 兼容读**：字段失去存在理由直接删，不用 `#[serde(rename)]`/双写/`unwrap_or(default)` 续命。读不到 key 就报错，不静默回退。
- **不写 `v0`/`legacy_`/`backward_compat` 模块**：任何以向后兼容为名的子模块/helper/trait/wrapper 直接拒。新枚举/新 schema 原地替换，删代码时同步删测试。

> 不确定要不要保留兼容层时，问：当前有没有外部用户的数据会因此被破坏？答案是「没有 / 用户可接受丢」——就按激进方向走。
