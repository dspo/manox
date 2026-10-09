# AGENTS.md

Guidance for coding agents working in this repo.（本文件是 manox 仓库指引的唯一权威；CLAUDE.md / CLAUDE.local.md 仅为指向本文件的入口，不承载内容。）

## 项目概述

manox 是**自研 agent runtime 库仓**（gpui-free、headless）：内核（harness）、宿主层（agent）、会话编排（session-core）、协议（protocol）、provider 配置（providers）、进程基础设施（supervisor）、终端仿真核心（terminal/hyperlinks）与 napi 绑定。对外以 git 依赖形式提供给 host——gpui 桌面应用与 cx 启动器在独立仓库 **dspo/manox-app** 维护（拆分点 tag `pre-manox-app-split`）。逐文件架构靠读代码获得——本文件只承载不可从代码推导的约束。

### 代码结构

```
crates/                    # Rust workspace 成员（全部 gpui-free）
  manox-agent/             # 核心 agent 逻辑（宿主层）
  manox-providers/         # LLM provider 配置与路由
  manox-harness/           # Trait agent 内核（core/ + ext/ 两子模块）
    src/core/              # 通用 agent 内核（纯内核，无业务逻辑）
    src/ext/               # 经内核拓展点扩展的业务能力
  manox-journal/           # 会话磁盘格式（journal v4 条目词汇）的叶子 crate
  manox-ahp/               # AHP 宿主层（频道、JSON-RPC、传输、x-manox 扩展面）
  manox-ahp-runtime/       # AHP 宿主背后的运行时半边（SessionRuntime 缝 + 适配器）
  manox-session-core/      # 会话核心（会话存储 + SessionRuntime 缝的实现 + loopback ws）
  manox-terminal/          # 终端仿真核心（自 manox-app 回流，UI 层 terminal-ui 在下游仓）
  hyperlinks/              # 终端超链接检测（随 manox-terminal 一同回流）
  supervisor/              # 子进程监督
  manox-lsp/               # LSP client 库（唯一零仓内消费者的成员，为宿主预留的积木：宿主层不集成，宿主 app 按需装配为工具或经 AHP session/activeClientSet 注册）
  manox-napi/              # napi 宿主绑定（休眠保留：VS Code 扩展已删除）
```

### 协议（AHP）

对外协议是 **AHP**（`ahp` / `ahp-types`，pin `=1.0.0`）：宿主层在 `manox-ahp`，
运行时半边在 `manox-ahp-runtime`，会话存储在 `manox-session-core`。
**自研的 "protocol v2" 已整体删除**（含 `crates/manox-protocol`）——v2 能做的
能力必须全部落在 AHP 面上（AHP 没有的那部分由 `x-manox/*` 扩展面承载），
否则该能力就是缺的，不是"v2 里还有"。

- `manox-agent` 永不认识 AHP：它只经 `ThreadHandle` / `BackendNotice` /
  `CapabilityClient` 暴露能力，协议适配一律在其上层。
- 依赖方向单向：`manox-ahp` → `manox-ahp-runtime` → `manox-session-core`。
  runtime 半边不得命名具体的会话运行时（进程单例的 builder 由拥有会话存储
  的一方安装），也不得反向依赖会话核心。
- 写操作 fail-closed：AHP 的 action 已被 reducer 折叠进订阅者状态，所以运行时
  拒绝必须回报为 refusal —— 静默 no-op 会让每个客户端收敛到一个从未发生的状态。

### 仓库边界（manox / manox-app 拆分）

- 本仓库**不得引入 gpui 或任何 UI 依赖**；apps、cx 启动器、ext-agents、terminal-ui（GPUI 渲染层）已迁至 `dspo/manox-app`。终端仿真核心 `manox-terminal` 与 `hyperlinks` 已回流本仓，由 manox-app 经 git 依赖消费。
- manox-app 经 git 依赖（branch=main + Cargo.lock 锁 rev）消费本仓库；本仓库 main 的每次 push 都可能被下游锁定引用——破坏性 API 变更须在 commit message 标注。下游约定：manox-app 合并前会把对本仓的依赖 bump 到最新兼容 commit，故 main 上任何提交都可能很快进入下游合并，保持 main 常绿。
- 本仓库运行时状态根 `~/.manox/` 与 manox-app 共享（见下）；协议/磁盘格式（journal、db、sessions）的变更即两仓联合变更。

## 构建与开发命令

```bash
cargo build
cargo test                           # live 测试用 MANOX_RUN_LIVE=1 env 门控，默认安全
MANOX_RUN_LIVE=1 cargo test          # 真实 API 测试（需 macOS Keychain 或 env 配 key）
cargo test -p manox-agent -- test_name     # 单 crate / 单测试
cargo clippy --all-targets
cargo fmt --all
script/gates.sh                      # 唯一门禁权威（fmt/clippy/test-real/test-clean；--quick 仅迭代用）
```

Rust **1.95.0**（`rust-toolchain.toml`），edition **2024**，需 `clippy`/`rustfmt`/`rust-src`。本仓库 headless，CI 无图形系统依赖。

## 字符串与语言（重要：开发时勿忘）

本仓库**不承载任何 i18n 设施**（无 Fluent 依赖、无 locales 资源、无语言配置、无 `.ftl`）：所有字符串——模板散文、工具 `description()`、工具 `run` 返回的 Err、日志、返回值、`thread.rs` 里 LLM 能读到的消息——**一律英文硬编码**，绝不本地化。

UI chrome 的本地化完全归下游 host（dspo/manox-app）所有，本仓库不提供文案键、不暴露 `t()` 类 API、不读取任何语言配置。新增面向模型或面向用户的可读字符串 = 直接写英文。凡**由本仓库传给 host 的值**（tool title/summary、slash 命令 `description`、plan 文本、模型产出内容），host 一律原样渲染，不做二次本地化；host 若要本地化自己的界面，按自己的键自行维护。

## 提示词系统

非必要不将提示词硬编码到 `.rs` 中，用 `.md` 文本文件维护：主 agent 提示词在 `crates/manox-agent/src/prompt/templates/system/*.tera.md`（Tera 渲染，`prompt/renderer.rs` 是唯一接触 `tera::` 的地方；模板只有英文单语一份，无 locale 子树）、子 agent 定义在 `crates/manox-harness/ext-agents/*.md`（`include_str!`，由 `ext/subagent/spawn.rs` 嵌入）、审批 reviewer 在 `crates/manox-agent/src/approval_agent_prompt.md`（`include_str!`，`approval_review.rs:16`）、标题生成在 `crates/manox-agent/src/title_agent_prompt.md`（`include_str!`，`title.rs:24`）；技能提示词 `skills/<name>/SKILL.md` 运行时从磁盘加载（`crates/manox-agent/src/skill.rs`）。短参数化模板（1-2 句）可留在 `.rs`，多段落散文一律用 `.md`。

## 运行时配置（`~/.manox/`）与生态根（`~/.claude/`）

持久化分两个根：**运行时状态统一位于 `~/.manox/`**（不再使用 `~/.config/cx/`）；**扩展生态资产位于 `~/.claude/`**（与 Claude Code CLI 物理共享，装一次两边可用；`MANOX_CLAUDE_HOME` 整体重定向，测试隔离用）。路径清单：

- 单一状态根：`~/.manox/`（`manox_agent::paths::manox_config_dir()` 与 `manox_providers::cx_state_dir()` 均指向它）。**多进程共享**：runtime 允许多实例共用同一状态根，跨进程协调在资源粒度——per-session 写租约（`manox_agent::session_lease`，`sessions/<id>.jsonl.lock`，驱动即独占、进程死自动释放、冲突报 `session/already-owned`）、threads.db 走 SQLite WAL、共享状态文件（registry/sidebar/sidecar）走 per-file flock、网关单例走 `~/.manox/gateway.lock`。无全 store 级单实例锁。
- 生态根：`~/.claude/`（`manox_agent::paths::claude_home()`）。用户技能 `~/.claude/skills/`、斜杠命令 `~/.claude/commands/`、子 agent 定义 `~/.claude/agents/`、规则 `~/.claude/rules/`（与 CLAUDE.md 层级一同由 `claude_md.rs` 装载）；插件与市场存储亦在此根下（见 `plugin.rs`）。
- LLM provider 配置：`~/.manox/cx.providers.config.yaml`（格式见 `crates/manox-providers`，Schema 见 `docs/cx/cx-config-schema.yaml`）；首启时会从旧根 `~/.config/cx/` 自动复制一次（旧文件保留）
- SQLite：`~/.manox/threads.db`（WAL 模式；`threads.db-shm` / `threads.db-wal` 随行）
- 线程 active-session 指针：`~/.manox/threads.registry.json`（thread → 当前驱动的 session 文件；Open/NewSession/恢复移动指针，侧栏按 thread 折叠其 sessions 为单行，`manox_agent::thread_registry`；跨进程 RMW 经 per-file flock 串行化）。工作目录随工具调用的 `cwd` 参数流动（sticky 继承 + `cwd_change` 条目持久化），无 worktree 会话 fork；多工作目录会话经 `CreateSession.workingDirectories` seed granted-root 围栏。
- 会话（.jsonl）：`~/.manox/sessions/`
- 子代理会话：`~/.manox/sessions/subagents/`（持久化、不进侧栏）
- 外部会话：`~/.manox/external-sessions/`（外部 CLI 会话由 manox-app 侧的 cx 驱动，目录约定在本仓库文档维护）
- 设置：`~/.manox/settings.toml`；主题：`~/.manox/themes/`
- 子 agent：`~/.claude/agents/*.md`（frontmatter name/description/tools/model + 正文；每个定义装配为一个独立委派工具：`ext/subagent/` 的 SubagentRuntime/SubagentProvider/能力协商/descriptor）；MCP：`~/.manox/mcp.json`（Claude Code `mcpServers` schema，stdio 或 HTTP；项目级 `.mcp.json` 与插件 `.mcp.json` 同 schema，合并序 plugin < global < project）；插件：`~/.manox/plugins/` + `~/.manox/marketplaces/` + `enabled_plugins.txt` / `disabled_plugins.txt`
- Plan 文件：`~/.manox/plans/`
- WS 网关端点（`cx web`，CLI 在 manox-app）：`~/.manox/gateway-ws.json`（0600；启动时写入 loopback 端口 + per-boot token，进程外客户端读它连 `ws://127.0.0.1:<port>/ws?token=…`；每次启动覆盖，进程退出后过期）。网关每机单例：`ws::start` 以非阻塞 flock 持 `~/.manox/gateway.lock`，他进程已持锁时本次 start 不绑定不发布（loud no-op）。
- ChromeUse profile：`~/.manox/chrome-profile/`（内置 Chrome 自动化引擎 `chrome_use` 的缺省 user-data-dir，登录态跨会话持久；可经 `settings.toml` 的 `[chrome]` 表改 executable / headless / user_data_dir / cdp_endpoint）
- cx CLI 状态：`~/.manox/cx.db`、IPC socket `~/.manox/sessions/`、codex 注入目录 `~/.manox/.codex/`、`~/.manox/.patch_source`（CLI 本体在 manox-app，状态目录与本仓库共享）
- API key 源：macOS Keychain（`keychain:SERVICE`）/ env（`env:VAR`）/ 字面量（`literal:...`）/ shell（`$(shell ...)`）
- **百炼 anthropic 兼容端点**（`*.aliyuncs.com/apps/anthropic`）：不报 `cache_creation_input_tokens`（恒 0），只报 `cache_read_input_tokens`。故 manox 的 `cache_creation` 记账对该端点恒 0 属预期，非解析/累加/持久化 bug（三链路均正确，记的就是端点报的 0）。`LastBreakpointOnly` 与 `Full` policy 对该端点均有效。

## crates/manox-harness 接线开发纪律（harness 分层）

manox 的 harness 是 manox-harness 内核（`crates/manox-harness/src/core`；老 manox harness 已退役并完全删除，代码存于 git 历史与 `origin/Manox` 备份分支）。接线开发遵循以下纪律：

### 分层与依赖链

`manox-agent（宿主）→ manox-harness/ext（扩展）→ manox-harness/core（内核）`；`manox-providers` 不进扩展层（仅服务 provider 路由域/外部 CLI 会话）。

- **crates/manox-harness/src/core 内核**：只承载通用 agent 内核能力（循环、压缩、会话、内建工具）+ 提供拓展点与拓展机制；宿主/业务逻辑一律不进内核。
- **crates/manox-harness/src/ext 扩展**：只经内核拓展点扩展业务能力（provider 自治注册、bash 编排、子代理、session sidecar、model_ref 等），不反向依赖宿主。
- **manox-agent 宿主**：装配 + manox 原创能力（审批策略、标题生成、斜杆命令路由、MCP 桥、Plan 模式等）。manox-harness 是唯一 harness 后端（harness 选择 feature 已移除）。

### 能力定层判定（每条新能力开工前必做）

按三分法定层：

1. **通用 agent 内核能力 → crates/manox-harness/src/core**：任何 agent harness 都需要的机制，wire 名/事件形状/serde 保真（例：compaction 事件、`prompt(text,{images})`、steer 带图、Input hook）。
2. **经内核拓展点可承载的业务能力 → manox-harness/src/ext**（不反向依赖宿主）。
3. **宿主原创能力 → 宿主层**（例：审批门控、MCP、标题生成、斜杆路由）；内核只留缝隙（如 `AgentTool::requires_approval`），不代行政策。
4. **定层偏离既有惯例必须显式注明理由**（写进 PR 的 Assumptions；例：manox-harness 的 MCP 工具比老 manox 更保守地过审批门控）。

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

- 独立 git worktree（`/private/tmp/manox--<branch>`）+ 常规分支命名（`feat/*` `fix/*` `hotfix/*` `release/*`，或个人前缀如 `dspo/*`）+ 正交 PR；发射点重叠时叠加 PR 并在 PR 中注明 base 关系与合入后 rebase 路径。
- 每 PR 门禁：以 `script/gates.sh` 的输出为唯一权威（含 `--quick` 之外的完整四腿），不做手挑 `cargo test` 的假绿声明。
- 已知沙箱环境性测试失败（harness 的 bind 类 provider 测试、IPC socket 测试）记录在案、不计回归；整机并发 timing flake 同样不计回归。**再记一条**（2026-09-28，CI ubuntu-latest 实测）：`manox_harness::core::session::jsonl::tests::test_message_entry_writes_camel_case_parent_id` 在 CI 全量跑中偶发 `Option::unwrap() on a None value`（`jsonl.rs` 的 `.find(...)` 找不到刚写的 `child` 行）。**实测样本**：同一 commit 三次 CI —— 一次绿、两次红，故是**非确定性**失败，不是稳定回归；同一 job 在本仓历史上也出现过别的并发类 flake（`concurrent_open_session_yields_one_entry_one_pump`、`concurrent_cold_appends_land_a_linear_chain`）。**本地无法复现**（macOS 上单独跑与全量跑各多次均恒过），故未定位到根因。已排查并排除的方向：该测试用独立 `tempfile::tempdir`，不共享状态；读路径持 shared `flock`、写路径持 exclusive `flock`，锁序正确；`#818` 对 `manox-harness` 的唯一改动是给 `PlanReview` 变体加两个 `#[serde(default)]` 字段（纯增量），而 `jsonl.rs` 根本不引用 `PlanReview`，**无因果通路**。判定：环境性/未定位，不计回归。**升级条件**：若它开始稳定失败（而非偶发），按真缺陷排查——优先怀疑「写完立刻从另一路径读回」的可见性窗口（`write_all` 后未 flush 即释放 fence 的 fd）。**再记一条**（2026-09-29，本机全量跑偶发、单跑与复跑恒过）：`manox_agent::engine::tests::journal_replay_is_consistent_across_disk_reload`——断言跨盘回读的 display 投影一致，时间戳为秒级精度，写入跨秒边界时两侧相差 1s 导致断言失败；与本 diff 无关，属测试自身的时钟边界脆性。**改一条**（2026-09-30）：旧条目 `manox_agent::monitor_bridge::monitor_spawn_bridges_snapshots`（整机并发 timing flake）未随 monitor_bridge 退场消失——该测试随宿主 TaskCenter 落地**改名为 `manox_agent::background_task::tests::host_observer_bridges_monitor_snapshots`**（测试体同形：真起 echo 命令监视器 + deadline 轮询通知），flake 记录对新名继续有效，观察勿从零重诊。**再记一条**（2026-09-30，本机全量 `cargo test --workspace --all-targets` 偶发、随后 16 轮 manox-agent lib 单跑 + 5 轮全量复跑恒过）：manox-agent lib 套件一次非确定性失败，测试名未能捕获（门禁日志经 tail 管道只留摘要）；发生在 PR codex/task-surface-pr3（HostTaskObserver 接线）门禁期间，同代码复跑不复现，暂记环境性/未定位，**升级条件**同上——若稳定失败按真缺陷排查，优先怀疑 observer 异步结算与同步 stop 终态推的 first-wins 竞争窗口。**流程补救**：门禁日志落盘（`> file`）而非管道 `| tail`，避免失败记录连测试名都留不下。**再记一条**（2026-10-08，gates 全量并发跑中一次非确定性失败、单跑与整包复跑恒过）：`manox_session_core::ahp_adapter_tests::dispatch::pending_message_set_parks_the_steer_under_its_id`——断言「steer 打进运行中的 turn 时回报 injected:true」实际拿到 `injected:false`（steer 到达早于 turn 真正开跑，测试自身的同步竞态），与 de-provenance diff 无因果通路（该 diff 未触碰 session-core 投递逻辑）；判定：满载 timing flake，不计回归，升级条件同上。
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
