# CLAUDE.md

Guidance for Claude Code working in this repo.

## 项目概述

manox 是**自研 agent runtime 库仓**（gpui-free、headless）：内核（harness）、宿主层（agent）、会话编排（session-core）、协议（protocol）、provider 配置（providers）、进程基础设施（supervisor/lsp）与 napi 绑定。对外以 git 依赖形式提供给 host——gpui 桌面应用与 cx 启动器在独立仓库 **dspo/manox-app** 维护（拆分点 tag `pre-manox-app-split`）。逐文件架构靠读代码获得——本文件只承载不可从代码推导的约束。

### 代码结构

```
crates/                    # Rust workspace 成员（全部 gpui-free）
  manox-agent/             # 核心 agent 逻辑（宿主层）
  manox-providers/         # LLM provider 配置与路由
  manox-harness/           # Trait agent 内核（core/ + ext/ 两子模块）
    src/core/              # TS Pi 内核移植（纯内核，无业务逻辑）
    src/ext/               # 经内核拓展点扩展的业务能力
  manox-protocol/          # 协议定义
  manox-session-core/      # 会话核心（AgentServer/Client 网关、journal、ws）
  supervisor/              # 子进程监督
  lsp/                     # LSP 集成
  manox-napi/              # napi 宿主绑定（休眠保留：VS Code 扩展已删除）
```

### 仓库边界（manox / manox-app 拆分）

- 本仓库**不得引入 gpui 或任何 UI 依赖**；apps、cx 启动器、ext-agents、terminal、hyperlinks 已迁至 `dspo/manox-app`。
- manox-app 经 git 依赖（branch=main + Cargo.lock 锁 rev）消费本仓库；本仓库 main 的每次 push 都可能被下游锁定引用——破坏性 API 变更须在 commit message 标注。
- 本仓库运行时状态根 `~/.manox/` 与 manox-app 共享（见下）；协议/磁盘格式（journal、db、sessions）的变更即两仓联合变更。

## 构建与开发命令

```bash
cargo build
cargo test                           # live 测试用 MANOX_RUN_LIVE=1 env 门控，默认安全
MANOX_RUN_LIVE=1 cargo test          # 真实 API 测试（需 macOS Keychain 或 env 配 key）
cargo test -p manox-agent -- test_name     # 单 crate / 单测试
cargo clippy --all-targets
cargo fmt --all
script/gates.sh                      # 唯一门禁权威（fmt/prod-libs/clippy/test-real/test-clean）
```

Rust **1.95.0**（`rust-toolchain.toml`），edition **2024**，需 `clippy`/`rustfmt`/`rust-src`。本仓库 headless，CI 无图形系统依赖。

## i18n（重要：开发时勿忘）

manox 区分**模型面向**与**用户面向**两条字符串边界：

1. **模型面向一律英文，绝不本地化**：`prompt/templates/{en,zh-CN}/**/*.tera.md` 模板散文、所有工具 `description()`、工具 `run` 返回的 Err 字符串、`thread.rs` 里 LLM 能读到的消息。永远不经 i18n。
2. **仅 UI chrome 本地化**（UI 在 manox-app，但其文案键与资源在本仓库）：经 `manox_agent::i18n::t("key")`。
3. **Fluent 资源**在 `crates/manox-agent/locales/{en,zh-CN}.ftl`，`include_str!` 编译期嵌入。**新增 UI 字符串 = 在两个 `.ftl` 各加一个键 + 调用处换 `t("key")`**，缺一不可。语言来自 `~/.manox/settings.toml`：UI 语言（`ui_language`）`manox_agent::init` 时读一次，可运行时热切换（`i18n::set_ui_language`，仅 chrome 重新本地化，已产生的内容不回改）；agent 语言（`agent_language`）按 thread 快照、终身不变。

## 提示词系统

非必要不将提示词硬编码到 `.rs` 中，用 `.md` 文本文件维护：主 agent 提示词在 `crates/manox-agent/src/prompt/templates/{en,zh-CN}/system/*.tera.md`（Tera 渲染，`prompt/renderer.rs` 是唯一接触 `tera::` 的地方）、子 agent 定义在 `crates/manox-harness/src/ext/agents/*.md`（`include_str!`）、审批 reviewer 在 `crates/manox-agent/src/approval_agent_prompt.md`（`include_str!`，`approval_review.rs:16`）、标题生成在 `crates/manox-agent/src/title_agent_prompt.md`（`include_str!`，`title.rs:24`）；技能提示词 `skills/<name>/SKILL.md` 运行时从磁盘加载（`crates/manox-agent/src/skill.rs`）。短参数化模板（1-2 句）可留在 `.rs`，多段落散文一律用 `.md`。

## 运行时配置（`~/.manox/`）

**所有持久化内容统一位于 `~/.manox/`**（不再使用 `~/.config/cx/`）。路径清单：

- 单一状态根：`~/.manox/`（`manox_agent::paths::manox_config_dir()` 与 `manox_providers::cx_state_dir()` 均指向它）
- LLM provider 配置：`~/.manox/cx.providers.config.yaml`（格式见 `crates/manox-providers`，Schema 见 `docs/cx/cx-config-schema.yaml`）；首启时会从旧根 `~/.config/cx/` 自动复制一次（旧文件保留）
- SQLite：`~/.manox/threads.db`（`threads.db-shm` / `threads.db-wal` 随行）
- 线程 active-session 指针：`~/.manox/threads.registry.json`（thread → 当前驱动的 session 文件；Open/NewSession/恢复移动指针，侧栏按 thread 折叠其 sessions 为单行，`manox_agent::thread_registry`）。工作目录随工具调用的 `cwd` 参数流动（sticky 继承 + `cwd_change` 条目持久化），无 worktree 会话 fork。
- pi 会话（.jsonl）：`~/.manox/sessions/`
- 子代理会话：`~/.manox/sessions/subagents/`（持久化、不进侧栏）
- 外部会话：`~/.manox/external-sessions/`（外部 CLI 会话由 manox-app 侧的 cx 驱动，目录约定在本仓库文档维护）
- 设置：`~/.manox/settings.toml`；主题：`~/.manox/themes/`
- 子 agent：`~/.manox/agents/*.md`（frontmatter name/description/tools/model/max_turns/allow_nesting + 正文）；MCP：`~/.manox/mcp.toml`（stdio 或 HTTP）；插件：`~/.manox/plugins/` + `~/.manox/marketplaces/` + `enabled_plugins.txt` / `disabled_plugins.txt`
- Plan 文件：`~/.manox/plans/`
- WS 网关端点（`cx web`，CLI 在 manox-app）：`~/.manox/gateway-ws.json`（0600；启动时写入 loopback 端口 + per-boot token，进程外客户端读它连 `ws://127.0.0.1:<port>/ws?token=…`；每次启动覆盖，进程退出后过期）
- ChromeUse profile：`~/.manox/chrome-profile/`（内置 Chrome 自动化引擎 `chrome_use` 的缺省 user-data-dir，登录态跨会话持久；可经 `settings.toml` 的 `[chrome]` 表改 executable / headless / user_data_dir / cdp_endpoint）
- cx CLI 状态：`~/.manox/cx.db`、IPC socket `~/.manox/sessions/`、codex 注入目录 `~/.manox/.codex/`、`~/.manox/.patch_source`（CLI 本体在 manox-app，状态目录与本仓库共享）
- API key 源：macOS Keychain（`keychain:SERVICE`）/ env（`env:VAR`）/ 字面量（`literal:...`）/ shell（`$(shell ...)`）
- **百炼 anthropic 兼容端点**（`*.aliyuncs.com/apps/anthropic`）：不报 `cache_creation_input_tokens`（恒 0），只报 `cache_read_input_tokens`。故 manox 的 `cache_creation` 记账对该端点恒 0 属预期，非解析/累加/持久化 bug（三链路均正确，记的就是端点报的 0）。`LastBreakpointOnly` 与 `Full` policy 对该端点均有效。

## crates/manox-harness 接线开发纪律（harness 分层）

manox 的 harness 已切换到 manox-harness 内核（`crates/manox-harness/src/core`，对标 `~/projects/github/pi` 的 TS Pi 上游；老 manox harness 已退役并完全删除，代码存于 git 历史与 `origin/Manox` 备份分支）。接线开发遵循以下纪律：

### 分层与依赖链

`manox-agent（宿主）→ manox-harness/ext（扩展）→ manox-harness/core（内核）`；`manox-providers` 不进扩展层（仅服务 provider 路由域/外部 CLI 会话）。

- **crates/manox-harness/src/core 内核**：只对标 TS Pi 核心能力 + 提供拓展点与拓展机制；宿主/业务逻辑一律不进内核。
- **crates/manox-harness/src/ext 扩展**：只经内核拓展点扩展业务能力（provider 自治注册、bash 编排、子代理、session sidecar、model_ref 等），不反向依赖宿主。
- **manox-agent 宿主**：装配 + manox 原创能力（审批策略、标题生成、斜杆命令路由、MCP 桥、Plan 模式等）。manox-harness 是唯一 harness 后端（harness 选择 feature 已移除）。

### 能力定层判定（每条新能力开工前必做）

先对照 `~/projects/github/pi`（TS 上游）与老 manox 实现（git 历史 / `origin/Manox` 分支）实证，再按三分法定层：

1. **TS pi 原生支持 → 照搬进 crates/manox-harness/src/core**（parity）：wire 名/事件形状/serde 保真（例：compaction 事件、`prompt(text,{images})`、steer 带图、Input hook；`HookPoint` 集即 TS extension 事件的镜像）。
2. **TS 无、pi 拓展点可承载 → manox-harness/src/ext**。
3. **TS 无、manox 原创 → 宿主层**（例：审批门控、MCP、标题生成、斜杆路由）；内核只留缝隙（如 `AgentTool::requires_approval`），不代行政策。
4. **偏离 TS 必须显式注明理由**（写进 PR 的 Assumptions，例：省略 `streamingBehavior`、manox-harness 的 MCP 工具比老 manox 更保守地过审批门控）。

### 内核纪律红线

- 内核不承载域字段：provider/模型配置走通用 metadata 通道，域字段不进内核类型。
- 内核不认宿主历史：风格别名/命名由宿主经 catalog/适配层注入（如 `LegacyAliasCatalog`）。
- 选择/路由逻辑归扩展层或宿主，内核只给机制。
- 同步 hook 不做异步 UI 往返：审批等异步交互在宿主 wrapper 实现（hook 只能同步阻断）。

### 退役代码处置

- 老 manox harness 曾退役为 `crates/harness-manox` 归档 crate；其中有价值的 harness 无关模块已迁入 agent 共享层（permission / approval_review / skill / command / frontmatter / proposed_plan / collaboration_mode / mcp 核心 / image / title）。归档 crate 本体已完全删除——需要参考老实现时查 git 历史或 `origin/Manox` 备份分支。

### 安全语义

- fail-closed：reviewer 不可用/超时/解析失败一律升级用户，绝不静默放行。
- 保守门控：远程/变更类工具默认过审批；always-allow 缓存按会话隔离。

### 工作流约定

- 独立 git worktree（`/private/tmp/manox--<branch>`）+ `codex/` 分支 + 正交 PR；发射点重叠时叠加 PR 并在 PR 中注明 base 关系与合入后 rebase 路径。
- 每 PR 门禁：`cargo clippy -D warnings --all-targets` + 全量 `cargo test` + `cargo fmt`（统一经 `script/gates.sh`）。
- 已知沙箱环境性测试失败（pi 的 bind 类 provider 测试、IPC socket 测试）记录在案、不计回归；整机并发 timing flake（`manox_agent::monitor_bridge::monitor_spawn_bridges_snapshots`：全量跑偶发失败、单独跑恒过）同样不计回归。
- PR 写清 Test Plan 与 Assumptions；注释必须准确描述代码（注释错位即回归，单独修复）。

## 项目规则

- **技术选型喜新厌旧**：能选最新 stable 就选最新 stable（依赖、工具链、API）。
- **禁止 vendor / submodule**：所有依赖经 Cargo 声明，不允许 vendor 目录或 git submodule。
- **crate 依赖只认 crate 索引或 git 地址**：外部 crate 只能是 crates.io 版本或 `git = "..."`，禁止 `path = "..."` 指向本机路径（CI 不可复现）；workspace 内部成员间 `path` 例外。
- **禁止抄袭第三方 crate 代码**：可参考架构思想，禁止复制粘贴后修改。`git2` 即因此被禁（plugin marketplace shell out 系统 `git`）。
- **注释一律英文，面向终态**（描述不变量/意图）而非过程流水账，非必要不注释。详见 `~/.claude/rules/code-comments.md`。
- **迭代时不得破坏前缀缓存**：provider 侧前缀缓存是透明优化（命中零成本，击穿静默回退）。任何对 `build_completion_request` 或消息组装管线的改动，必须保持跨 turn 请求前缀字节一致；若需重写历史，须先接入 `AppendOnlyContextManager`（`prefix_stability.rs`）或显式禁用该路径缓存。
- **零构建告警**：CI 以 `-D warnings` 编译。提交前必须本地 `cargo clippy --all-targets -- -D warnings` 全绿、`cargo build` 无 warning。新增 `#[allow(...)]` 视为逃避而非修复，除非该 lint 本身与项目设计冲突，且必须在 `#[allow]` 处用英文注释说明。`Result` 必须 `let _ =` 或 `?` 处理，禁止裸丢弃；test 模块必须在文件末尾。
- **勿以善小而不为**：对正面有效的 review 意见，即便不构成阻塞也应尽量遵从。
- **Plan 应写入实施方式**：制定涉及编码的 Plan 时，将「按 /gitwork:deliver 实施」写入 Plan 正文。

## 激进开发纪律

manox 处于开发早期，不维护 v0→v1 升级路径，不背历史负债。

- **运行时禁止 schema migration**：运行时永不改 DDL、不删除/重建用户机器上的 db。`db/mod.rs` 的 `open()` 仅对全新 db `CREATE TABLE IF NOT EXISTS`，对已有 db 是 no-op。开发中的 schema 迁移，直接手动改开发机 db（`sqlite3` CLI / `ALTER TABLE` / 重建）。schema 不对就该报错报错、该 panic panic。
- **不保留兼容字段 / 不写 fallback 兼容读**：字段失去存在理由直接删，不用 `#[serde(rename)]`/双写/`unwrap_or(default)` 续命。读不到 key 就报错，不静默回退。
- **不写 `v0`/`legacy_`/`backward_compat` 模块**：任何以向后兼容为名的子模块/helper/trait/wrapper 直接拒。新枚举/新 schema 原地替换，删代码时同步删测试。

> 不确定要不要保留兼容层时，问：当前有没有外部用户的数据会因此被破坏？答案是「没有 / 用户可接受丢」——就按激进方向走。

## δ₂: manox-actor retired + VS Code protocol migration (v4 delta 2)

- manox-actor crate deleted (2026-08-31).
- manox-napi now uses AgentServer + InProcessConnection instead of manox-actor.
- VS Code extension adapted to FromClient/FromServer protocol (AgentServer wire).
- handle_command/ActorState/SessionState/EventSink/thread_event_to_json/spawn_models_push deleted from session-core.
