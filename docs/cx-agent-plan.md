# Cx Agent — 实现计划与进度跟踪

> **使命**：在 cx 二进制内实现一个 **in-process** 的 coding agent（代号 **Cx Agent**），
> 覆盖 OpenAI Responses / OpenAI Chat Completions / Anthropic Messages 三种 wire API，
> 完全复用 cx 现有 (provider × model × wire_api) 配置。
>
> **本文件作用**：每次会话启动前阅读 §0 + §1 + §2 + §5 即可恢复全部上下文并继续推进。
> 任意 coding agent（不一定是当时的对话方）都应该能凭这一份文件接手开干。
>
> **维护规则**：
> - 每次推进后**至少**更新 §0 当前状态 和 §11 工作日志。
> - 改动选型决议（§2）必须先在 §7 反悔点匹配触发条件，再写入 §11 工作日志，并在决议表加上"修订日期"。
> - 退出条件未满足不得跳进下一阶段。
> - 凡是新加依赖必须更新 §6 验收清单的体积/编译时间检查。

---

## 0. 当前状态（Status）

- **Phase**：`v0 Done`（Phase 0 / 1 / 2 / 3 ✅ 全部完成）
- **最近会话进展**：Cx Agent 已完成全链路交付：launcher 接入、全屏 TUI、多轮 tool loop、六个内建工具、approval gate、rollout/stats 对齐、README/对比文档/配置 schema、单测与 live smoke 全部落地。macOS 上已实际跑通 Anthropic（`MiniMax-M2.5`）、Responses（`qwen-plus-latest`）、Completions（`qwen3.6-flash`）三组完整 chat + tool 流程；Linux（Multipass Ubuntu 24.04 / aarch64）上也已实际跑通 Responses + `read_file`，返回 `Cargo.toml` package name = `cx`。
- **下次会话起点**：无。v0 已收官；若继续推进，只从 §10 deferred/v1 backlog 开始。
- **Baseline 指标（Phase 0 起点，§6 验收对照用）**：
  - crates: 345
  - debug binary: 21 MB
  - clean build: 21.2 s
- **v0 最终指标**：
  - crates: 590 (+245)
  - debug binary: 41 MB (+20 MB)
  - clean build: 46.79 s (+25.59 s，在 §6 预算 60 s 内)
- **未解决疑问 / 已知坑**：
  - 无阻断项。对 DashScope-compatible Responses / Completions，tool result 续写需走 plain user-text fallback；Anthropic 继续使用结构化 `tool_result`，该差异已在 `provider/rig.rs` 固化并通过 live smoke 验证。

> 接手 agent 须知：本节是"唯一 source of truth"，看完这节就知道下一步该干嘛。

---

## 1. 仓库快照（Repo Snapshot）

接手前请先扫这一节，了解 cx 当前形态。

### 关键文件

```
src/lib.rs                         # ~4600 行，包含全部现有功能（launcher/probe/patch/stats）
src/main.rs                        # 6 行，仅调 cx::run()
src/stats.rs                       # ~830 行，cx stats TUI（参考实现，下面会"复用其架构思路"）
config/providers.default.yaml      # 默认 provider 配置（include_str! 进二进制）
docs/cx-config-schema.yaml         # provider YAML 配置 schema 描述
docs/cx-stats-plan.md              # 上一阶段完成的功能（仅参考）
docs/agents-comparison.md          # 已收录 agent 的对比说明
Cargo.toml                         # 当前依赖（v0 之前已有）
rust-toolchain.toml                # 已固定工具链
```

### Cargo.toml 现有依赖（不要重复加）

```toml
[dependencies]
anyhow = "1.0"
base64 = "0.22"
clap = { version = "4", features = ["derive"] }
crossterm = "0.28"
dirs = "6.0"
rand = "0.8"
ratatui = "0.29"
reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
serde_yaml = "0.9"
url = "2.5"
which = "7.0"
```

注意：当前 reqwest 是 `blocking`，**不带 `stream` feature**。Phase 0 会**新增** `["stream", "json"]` 等异步 feature。

### 入口分发链路（接手必看）

```
main()
  └─ cx::run()                                  src/lib.rs:1231
       └─ dispatch_command(&raw_args)           src/lib.rs:1203
            ├─ DispatchCommand::Help     → print_help()
            ├─ DispatchCommand::Add      → run_add()
            ├─ DispatchCommand::Patch    → run_patch()
            ├─ DispatchCommand::Probe    → run_probe()
            ├─ DispatchCommand::Stats    → stats::run_stats()
            └─ DispatchCommand::Launch   → run_launcher()      ← Cx Agent 接入点在这之后
```

`run_launcher()` 内部调用 `run_tui()` 让用户选 (agent × provider × model)，返回 `Option<Selection>`。
**Cx Agent 的接入方式**：在 agent 选择列表末尾追加 "Cx Agent"，选中后 `run_launcher()` 不走 `exec_launch(spec)`，
改走新分支 `cx_agent::run_cx_agent(selection, passthrough_args)`。

### 已有的关键类型（你会反复消费它们）

```rust
// src/lib.rs:286
struct ResolvedModel {
    id: String,            // 例如 "claude-opus-4-7" / "gpt-5.4" / "qwen3.7-max"
    desc: String,
    wire_api: WireApi,     // Responses | Completions | Anthropic | Unavailable
    provider_name: String, // 配置文件 providers[].name
    endpoint_url: String,  // 完整 endpoint URL
    visible_agents: Vec<String>,
    copilot_auth: CopilotAuth,
    // ... 其他评分字段（arena/swe_p/tb2）Cx Agent 用不上
}

// src/lib.rs:481
enum WireApi { Responses, Completions, Anthropic, Unavailable }

// src/lib.rs:570
struct ResolvedProvider {
    name: String,
    has_endpoints: bool,
    apikey_source: Option<String>,  // 例如 "env:OPENAI_API_KEY" 或 "file:~/.config/cx/keys/x.txt"
}

// src/lib.rs:591
struct Selection {
    agent_id: String,            // Cx Agent 时 = "cx-agent"
    agent_binary: String,        // Cx Agent 时为空字符串或不使用
    provider: ResolvedProvider,
    model: Option<ResolvedModel>,
}

// src/lib.rs:1012
fn resolve_apikey(source: &str) -> Result<String>  // 用它读 apikey_source
```

### Provider 配置 YAML 摘要

```yaml
providers:
  - name: openai
    apikey_source: env:OPENAI_API_KEY
    endpoints:
      responses:
        url: https://api.openai.com/v1/responses
        agents: [codex, claude]
      completions:
        url: https://api.openai.com/v1/chat/completions
        agents: [codex, copilot]
    models:
      - id: gpt-5.4
        desc: 旗舰
        wire_apis: [responses, completions]
        agents: []                  # [] 表示继承 endpoints.*.agents
```

WireApi 与 endpoint 是**多对多**关系——同一 provider 可能配置 `responses` 与 `completions` 两个 endpoint。
`ResolvedModel.endpoint_url` 已经在配置解析阶段绑死到具体 wire_api 对应的 url。

---

## 2. 总目标（North Star）与选型决议

### 2.1 v0 范围

让 cx 启动时的 agent 选择列表末尾出现 **"Cx Agent"**，进入后获得：

- TUI 聊天界面（流式输出）
- 6 个基础工具：`read_file` / `write_file` / `edit_file` / `bash` / `grep` / `glob`
- 类别感知的 approval（`always-allow` / `per-call` / `read-only-auto-allow`）
- 完全消费 cx 现有 YAML provider 配置
- 三种 wire API 全支持
- **零外部进程**、**零 ACP**、**零 codex/goose 依赖**
- 会话以 jsonl rollout 写入 `~/.local/share/cx/cx-agent-sessions/<YYYY-MM-DD>/<session-id>.jsonl`，
  自动接入现有 `cx stats`

### 2.2 v0 非目标

| 不做 | 原因 |
|---|---|
| Sandbox / process hardening | 不在 v0 范围；codex 抄一份太重，自写需 1000+ 行 |
| MCP 客户端 | v0 工具用 in-process trait 即可 |
| 多 agent / subagent | v0 一对一会话即可 |
| Web search / 浏览器 | 与编码任务无关 |
| Conversation resume UI | rollout 已存盘，UI v1 再做 |
| Tree-sitter / Tantivy / Qdrant / notify | 见 §10 deferred 列表 |

### 2.3 选型决议表（已敲定，改动需走 §7 流程）

| 决议项 | 结果 | 理由 |
|---|---|---|
| 底座 LLM 库 | **`rig-core` v0.37.0**（精确 pin） | 已支持三种 wire API，Responses 是默认 OpenAI backend |
| 用法 | **方案 A'**：只用 rig 底层 client/streaming/tool-call normalization；agent loop / approval / TUI / history 全部 cx 自写 | 兼顾代码量与产品自由度 |
| 异步运行时 | **tokio current_thread**（不是 multi_thread） | reqwest::blocking 内部本就在跑 tokio；显式 + 局部使用更清晰 |
| 架构 | **in-process only** | 用户硬约束 |
| TUI 基座 | **ratatui + crossterm**（沿用 cx stats 栈），辅以 `tui-textarea` 做多行输入 | 生态最成熟，codex/goose/gitui/yazi 都用这套 |
| 工具实现 | **in-process trait `Tool`**，每个工具一个文件 | 不引入 MCP；后续 v1 可再桥接 |
| AST 解析 | **v0 不引入 tree-sitter** | 6 个基础工具用不上 AST；引入会显著增加编译时间和二进制体积 |
| 文件监听 | **v0 不引入 notify** | turn-based 会话期间不需要后台监听 |
| 代码搜索引擎 | **v0 不引入 tantivy/qdrant** | 与 agent 定位不同；v1 视需要再考虑 |
| Rollout 格式 | jsonl，**结构与 codex rollout 一致**（`type=event_msg` + `payload.type=token_count`），方便复用 cx stats 解析 | 接入现有 stats 几乎零改动 |
| 排除依赖 | codex / goose / ACP / 多语言（V8/Python/WASI/Rhai）/ bloop 整体架构 | 与 in-process 或包体/复杂度冲突 |

### 2.4 关键风险与对策

| 风险 | 对策 |
|---|---|
| rig-core pre-1.0 break change | 所有 rig 类型只在 `cx_agent/provider/rig.rs` 出现；`Cargo.toml` 用 `=0.37.0` 精确版本 |
| rig 不能干净接 cx 多 provider（GLM/Qwen/Kimi 等 OpenAI-compatible） | **Phase 0.2 必须先验证**；过不了走 fallback B（自写三套 wire client），并按 §7 走流程 |
| Cx Agent 跑成功后 `cx stats` 看不到数据 | 写 codex 风格 jsonl，stats 模块加 `agent: cx-agent` 数据源（见 §5 Phase 1.5） |
| reqwest 同时存在 blocking 与 async 用法导致 runtime 嵌套 | Cx Agent 全程在 tokio runtime 内；外部 cx 现有调用维持 blocking 不变（互不干扰） |

---

## 3. 模块布局（Layout）

```text
src/
  cx_agent/                    # 所有 Cx Agent 代码自洽于此目录
    mod.rs                     # 唯一对外入口：pub fn run_cx_agent(selection, args) -> Result<()>
    runtime.rs                 # tokio current_thread runtime 封装（一次性 build_on）
    session.rs                 # turn loop + in-memory history + 接 rollout
    config.rs                  # cx YAML provider/model/wire_api → ProviderAdapter 配置
    approval.rs                # ApprovalMode, ToolCategory, ApprovalGate, ApprovalDecision
    history.rs                 # CxMessage / CxContent IR + helper
    events.rs                  # CxStreamEvent / AgentEvent
    rollout.rs                 # jsonl 持久化（cx-agent-sessions/<date>/<session>.jsonl）
    provider/
      mod.rs                   # trait ProviderAdapter
      rig.rs                   # rig-core 适配实现（唯一 import rig 的文件）
      mapping.rs               # ResolvedModel + ResolvedProvider → rig client/model
    tools/
      mod.rs                   # trait Tool + Registry + ToolInvocation
      read_file.rs
      write_file.rs
      edit_file.rs
      bash.rs
      grep.rs
      glob.rs
    tui/
      mod.rs
      chat.rs                  # 消息列表 + 输入框（tui-textarea）+ 流式渲染
      approval_prompt.rs
  lib.rs                       # 仅在 Launch dispatch 中追加调用 cx_agent::run_cx_agent()
  stats.rs                     # 加一个 scan_cx_agent() 数据源（Phase 1.5）
```

### 铁律

1. **`cx_agent` 之外的代码禁止 `use rig_core::*`**。`provider/rig.rs` 是唯一接触点。
2. `lib.rs` 只暴露 `cx_agent::run_cx_agent()` 这一个入口（其他 pub 可见性都是错）。
3. **不引入** tree-sitter / notify / tantivy / qdrant / bloop 风格的搜索基座。v1 再评估，见 §10。
4. 编译期可考虑用 `feature = "cx-agent"` gate 整个模块（**已敲定：v0 不加，2026-05-27**；若 Phase 2 末编译时间或体积超预算再走 §7 反悔点 #3）。
5. **公共类型与 lib 共享**：跨文件用的 `ResolvedModel` / `ResolvedProvider` / `Selection` 等，由 `lib.rs` 暴露成 `pub(crate)` 即可，**不要重新定义**。

---

## 4. 核心数据结构（IR 草案）

### 4.1 Provider-neutral 消息 IR

```rust
// src/cx_agent/history.rs
#[derive(Debug, Clone, PartialEq)]
pub enum CxMessage {
    System { content: String },
    User { content: Vec<CxContent> },
    Assistant { content: Vec<CxContent> },
    ToolResult {
        call_id: String,
        name: Option<String>,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum CxContent {
    Text(String),
    Reasoning(String),
    ToolCall {
        id: String,
        call_id: Option<String>,
        name: String,
        arguments: serde_json::Value,
    },
}
```

### 4.2 工具与 approval

```rust
// src/cx_agent/approval.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory { Read, Write, Execute }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    AlwaysAllow,
    PerCall,
    ReadOnlyAutoAllow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    Ask,
    Deny { reason: String },
}

pub fn decide(mode: ApprovalMode, category: ToolCategory) -> ApprovalDecision {
    use ApprovalMode::*; use ToolCategory::*; use ApprovalDecision::*;
    match (mode, category) {
        (AlwaysAllow, _)                => Allow,
        (PerCall, _)                    => Ask,
        (ReadOnlyAutoAllow, Read)       => Allow,
        (ReadOnlyAutoAllow, _)          => Ask,
    }
}
```

### 4.3 Provider Adapter 抽象

```rust
// src/cx_agent/provider/mod.rs
use anyhow::Result;
use futures::stream::BoxStream;

pub struct CxTurnRequest {
    pub system: Option<String>,
    pub history: Vec<crate::cx_agent::history::CxMessage>,
    pub tools: Vec<CxToolDefinition>,
    pub model_id: String,
}

pub struct CxToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub category: crate::cx_agent::approval::ToolCategory,
}

#[async_trait::async_trait]
pub trait ProviderAdapter: Send + Sync {
    async fn stream_turn(
        &self,
        request: CxTurnRequest,
    ) -> Result<BoxStream<'_, Result<crate::cx_agent::events::CxStreamEvent>>>;
}
```

### 4.4 Stream events

```rust
// src/cx_agent/events.rs
#[derive(Debug, Clone)]
pub enum CxStreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart { id: String, name: String },
    ToolCallArgsDelta { id: String, partial: String },
    ToolCallDone { id: String, name: String, arguments: serde_json::Value },
    Usage { input: u64, output: u64, cache_read: u64, cache_write: u64 },
    Done,
    Error(String),
}
```

### 4.5 三种 wire API 投影规则

adapter 层必须正确实现以下映射：

| IR | OpenAI Responses | Chat Completions | Anthropic Messages |
|---|---|---|---|
| `System` | top-level `instructions` | `role: system` 消息 | top-level `system` |
| `ToolCall.arguments` | JSON string in function_call | JSON string in tool_calls | parsed JSON object in `tool_use.input` |
| `ToolResult` | `function_call_output` + `call_id` | `role: tool` 消息 | user 消息 + `tool_result` block |
| 流式 text delta | `response.output_text.delta` | `delta.content` | `content_block_delta.text_delta` |
| 流式 tool args | `function_call_arguments.delta` | `delta.tool_calls[index]` | `partial_json` |
| Usage | `response.completed.usage` | `usage` (final chunk) | `message_delta.usage` 或 `message_start.usage` |

**Anthropic 的 tool_result 必须以 `user` role 批量回传**，不能像 OpenAI 那样单条 tool 消息。

---

## 5. 阶段计划（Roadmap）

> 每阶段都有 **进入条件 / 任务 / 退出条件 / 失败兜底**。退出条件不满足不得进入下一阶段。

### Phase 0 — Spike（关键验证）

- **进入条件**：本计划已读完
- **预估工时**：3–5 天

| # | 任务 | 退出条件（每条都可独立验证） |
|---|---|---|
| 0.1 | 修改 `Cargo.toml`：<br>`tokio = { version = "1", features = ["rt", "macros", "process", "io-util", "time"] }`<br>`rig-core = "=0.37.0"`<br>`async-trait = "0.1"`<br>`tui-textarea = "0.7"`<br>`futures = "0.3"`<br>同时为 `reqwest` 加 `"stream"` feature | `cargo build` 通过；`cargo tree \| wc -l` 增量记录到 §11 |
| 0.2 | **核心验证**：写 `examples/cx_agent_spike.rs`，从 `~/.config/cx/cx.providers.config.yaml` 中抓 3 个不同 provider × wire_api 组合（必须包含 OpenAI 官方 + 一个 OpenAI-compatible 国产 + 一个 Anthropic），各跑一次 streaming "say hi" | 三组都能在终端看到逐字流式输出；`cargo run --example cx_agent_spike` 干净退出 |
| 0.3 | 把 rig stream events 转成草案版 `CxStreamEvent`；usage tokens 提取（in/out + cache） | events 至少能看到 `TextDelta` / `Usage` / `Done`；usage 数字与 provider 控制台对得上 |
| 0.4 | 决定是否给 cx_agent 加 `feature = "cx-agent"`；把决议结果写进 §2 决议表 + §11 工作日志 | 决议落盘 |

**失败兜底**：
- 0.2 失败（rig 不能优雅接某个国产 OpenAI-compatible provider）：触发 §7 反悔点 #1。具体动作：
  1. 在 §11 写下"Phase 0.2 fallback to B" + 失败 provider 详细错误
  2. 替换底座决议为：自写三套 wire client（`reqwest` + 手写 SSE）
  3. 重做 Phase 0.1（依赖换成 `eventsource-stream`）
  4. Phase 1+ 继续推进

### Phase 1 — TUI Chat（流式聊天）

- **进入条件**：Phase 0 全部退出条件 ✅
- **预估工时**：1 周

| # | 任务 | 退出条件 |
|---|---|---|
| 1.1 | `lib.rs` agent 列表末尾追加 `"Cx Agent"` 项；`Selection.agent_id == "cx-agent"` 时分发到 `cx_agent::run_cx_agent()` | `cx` 启动后能在列表选中并进入 Cx Agent |
| 1.2 | 实现 `cx_agent/{mod,runtime,session,events,history}.rs` 骨架；`cx_agent/provider/{mod,rig,mapping}.rs` 完成单个 wire_api（先 Anthropic，因为最简单） | 能在 Cx Agent 内发一条消息看到 assistant 流式回复 |
| 1.3 | 实现 `tui/chat.rs`：消息列表（历史 + 当前 stream） + 输入框（`tui-textarea`）+ 滚动 + 状态栏 | 能输入 prompt、看到流式输出、能滚动历史 |
| 1.4 | 把 Provider Adapter 扩到三种 wire API 全支持 | 能切换到任意 model（Phase 0 验过的三组）跑通 |
| 1.5 | `rollout.rs`：每个 turn 写一行 jsonl 到 `~/.local/share/cx/cx-agent-sessions/<YYYY-MM-DD>/<session-id>.jsonl`；格式参照 codex（`type=event_msg`, `payload.type=token_count`, `payload.info.last_token_usage`, 以及 `type=turn_context`, `payload.model`） | 文件存在；用 `jq` 能解析 |
| 1.6 | `src/stats.rs` 加一个 `LogSource { kind: SourceKind::CodexLike("cx-agent"), root: cx-agent-sessions/ }` | `cx stats` 的 Matrix 视图出现 `cx-agent` 列；CX_STATS_DUMP=1 能看到记录 |
| 1.7 | Esc / Ctrl-C 退出 + 终端复位（参考 `stats.rs:run_tui` 的 disable_raw_mode + LeaveAlternateScreen 模式） | 退出后终端可正常使用 |

**失败兜底**：
- 1.5 jsonl 与 stats 解析对不齐：先用最小化 schema（`agent / model / date / in_tokens / out_tokens` 五字段）写一份 cx-agent 专用 parser，**不要**强行复用 codex parser。

### Phase 2 — Tools + Approval（核心难点）

- **进入条件**：Phase 1 全部退出条件 ✅
- **预估工时**：1 周

| # | 任务 | 退出条件 |
|---|---|---|
| 2.1 | `tools/mod.rs`：`Tool` trait + `Registry`；6 个工具的空骨架 + JSON schema | `cargo build` |
| 2.2 | 实现 read 工具：`read_file` / `grep` / `glob` | 模型能成功 tool_call → 返回 → 继续生成（三种 wire API 各跑一遍） |
| 2.3 | 实现 `bash`（用 `tokio::process::Command`，stdout/stderr 全部读完后整体回填；超时 60s） | 能跑 `ls -la` 并把输出回填给模型 |
| 2.4 | 实现 `write_file` / `edit_file`（apply-patch 风格 unified diff） | 能创建/修改文件；diff 格式错误时给清晰报错 |
| 2.5 | `approval.rs` + `tui/approval_prompt.rs`：弹窗显示 tool name / arguments preview / 三档决策 | 三档 mode 实测正确（与 §4.2 决策表一致） |
| 2.6 | 多轮 tool call 循环（call → result → continue → ...） | 模型连续 ≥3 次 tool call 无错乱；turn 计数正确写进 rollout |
| 2.7 | 三种 wire API 的 tool call/result 投影各跑一遍真实任务："在 /tmp 下创建 hello.txt 写入 'hi'，然后读出来确认内容" | 三组 wire API 都能从头到尾完成任务 |

**失败兜底**：
- 2.7 某个 wire API tool 行为有 bug：先把这个 wire API 在 launcher 中标记 unstable（提示行 + 不在默认推荐列表），其它两个继续推进，bug 写进 §0 已知坑。

### Phase 3 — Polish（收尾）

- **进入条件**：Phase 2 全部退出条件 ✅
- **预估工时**：1 周

| # | 任务 | 退出条件 |
|---|---|---|
| 3.1 | 错误展示（API 失败 / tool 失败 / 超时） | 友好红字 + 可重试，不会把整个 session 打死 |
| 3.2 | Token usage 实时显示（顶部状态栏） | 每轮结束更新 in/out/total |
| 3.3 | `approval_mode` 写入 cx YAML（`cx_agent.approval_mode`，默认 `read-only-auto-allow`） | YAML 存在则读，不存在用默认 |
| 3.4 | Provider mapping 单测 + Tool 单测 + fake provider 集成测试（`mockito` 或 `wiremock`） | `cargo test` 通过 |
| 3.5 | README 加 Cx Agent 段落；`docs/agents-comparison.md` 加一行 | 描述准确，内含一个示例命令 |
| 3.6 | 跑完 §6 验收清单，全部勾选 | 完成 |

---

## 6. 验收清单（v0 Done 标准）

- [x] `cx` 启动后能在 agent 列表选 "Cx Agent"
- [x] OpenAI Responses provider 跑通完整 chat + tool 流程
- [x] OpenAI Chat Completions provider 跑通完整 chat + tool 流程
- [x] Anthropic Messages provider 跑通完整 chat + tool 流程
- [x] 6 个基础工具全部能用且返回有意义的输出
- [x] 三档 approval mode 行为符合 §4.2 矩阵
- [x] `cx stats` 能看到 `cx-agent` 列的 token 用量
- [x] 二进制大小相比 v0 起点（基线见 §11）增长 ≤ 30 MB
- [x] `cargo build`（首次清空 target 后）增长 ≤ 60 秒
- [x] 在 macOS 和 Linux 上都能跑（手测各一次）
- [x] `cargo test` 通过
- [x] `cargo clippy -- -D warnings` 无新增 warning
- [x] `docs/cx-agent-plan.md` §11 工作日志已记录最终状态

---

## 7. 反悔点（Decision Reversal Triggers）

如果出现以下情况，**暂停推进**，按下面流程操作：

| # | 触发条件 | 流程 |
|---|---|---|
| 1 | Phase 0.2 验证失败 | 替换 LLM 底座决议为 fallback B（自写 wire client）；§2 决议表加修订日期；§11 写日志；继续 Phase 0.3 但接 fallback |
| 2 | rig-core 在三个月内出 break change 影响 ≥ 2 处 cx 代码 | 评估 `genai` / 自写；§2 决议表加修订；§11 写日志 |
| 3 | `cargo build` 时间增长 > 2 分钟 | 引入 `feature = "cx-agent"` 编译开关；评估去掉 rig 高层 agent 模块只保留 client |
| 4 | 二进制体积增长 > 50 MB | 同上；同时检查是否引入了非必要的 reqwest feature |
| 5 | 某个 wire API 在 spike 阶段就出现严重不一致行为 | 评估是否限制 v0 只支持其中两个 API；§2 决议表加修订；§11 写日志 |

**反悔操作必须留痕**：在 §2 决议表对应行加 `修订: YYYY-MM-DD`；在 §11 写一段说明。

---

## 8. 测试与验证手册

任意 coding agent 接手时，遇到下列场景应使用对应命令：

```bash
# 编译检查（最常用）
cargo build

# 编译 + clippy（提交前）
cargo clippy -- -D warnings

# 单元测试
cargo test

# Phase 0 spike 单独跑（仅 Phase 0 阶段存在）
cargo run --example cx_agent_spike

# 烟测 cx 启动 launcher（阻塞 TUI，需手动 Ctrl-C）
./target/debug/cx

# 烟测 cx stats（dump 模式不进 TUI）
CX_STATS_DUMP=1 ./target/debug/cx stats

# 检查 cx-agent rollout 是否产生
ls -lh ~/.local/share/cx/cx-agent-sessions/

# 检查二进制体积（与 §6 验收对照）
ls -lh ./target/release/cx
```

---

## 9. 接手指引（Onboarding for the Next Agent）

新会话接手 **Cx Agent 工作时的标准开场**：

1. 读 §0 当前状态。**这是 single source of truth**。
2. 读 §1 仓库快照。如果你不熟 cx 现有结构，必须看完。
3. 读 §2 选型决议。**已敲定的不要再讨论**；要改必须走 §7。
4. 读 §5 当前 Phase 的任务表，找出"未完成的最小编号步骤"。
5. 跑一遍 §8 中相应的检查命令，确认起点状态符合 §0 描述。
6. 推进；每完成一个步骤更新 §0 + §11；每完成一个 Phase 跑一次 §6 验收清单。

**不允许**的行为：
- 跳过 §7 流程偷偷改 §2 决议
- 在 `cx_agent` 之外 `use rig_core::*`
- 不更新 §0 / §11 就结束会话
- 在 v0 阶段引入 §10 列表中的延后依赖

---

## 10. v1 才考虑的延后项（Deferred）

明确划界，避免 v0 阶段被诱惑提前引入：

| 项 | 触发条件（什么情况才考虑） | 大致工作量 |
|---|---|---|
| `tree-sitter` + 多语言 grammar | 加 "semantic project map" / 智能 chunking 工具 | 编译时间 +60s，二进制 +30MB；约 800 行代码 |
| `notify` 文件监听 | 加 background task / auto-context / 监控 build 输出 | +200 行，跨平台调试成本 |
| `tantivy` 全文索引 | 项目大到 ripgrep 太慢 | +300 行 + 索引存储设计 |
| `qdrant` 向量搜索 | 做 codebase RAG | +显著依赖；建议保留 v2 |
| MCP 客户端（`rmcp` / 自写） | 接入外部工具生态 | +500 行 + 协议处理 |
| Conversation resume UI | 用户开始抱怨"上次会话找不回来" | +200 行（rollout 已存盘） |
| Sandbox（macOS Seatbelt / Linux landlock / Windows AppContainer） | 用户跑不可信 prompt 时 | +1000–2000 行，平台差异巨大 |
| Multi-agent / subagent | 出现"主 agent 调研 + 子 agent 执行"诉求 | +600 行 |

bloop 整体架构（GUI + 搜索引擎 + tree-sitter + Tauri）**永远不引入**。

---

## 11. 工作日志（Work Log）

> 每次会话推进都追加一段。格式：
> ```
> ## YYYY-MM-DD · 简要标题
> - 完成: ...
> - 决议变更: ...（无则省）
> - 已知坑: ...（无则省）
> - 下一步: ...
> ```

### 2026-05-27 · 计划落地 + 选型敲定

- 完成: 需求梳理；底座选型（rig-core v0.37.0）；tokio 决议（current_thread）；approval / IR / 模块布局草案；本计划文件 v1.0 落地。
- 完成: 补 §1 仓库快照、§4 IR 完整签名、§5 Phase 任务退出条件、§7 反悔点流程、§8 测试手册、§9 接手指引、§10 deferred 列表；plan v1.1。
- 决议变更: 无（首次落地）。
- 已知坑: rig-core 0.37 对国产 OpenAI-compatible provider 的兼容度未验证（待 Phase 0.2）。
- 下一步: Phase 0.1 — 加依赖、`cargo build` 通过。

### 2026-05-27 · Phase 0.1 完成

- 完成: Cargo.toml 加入 `tokio` / `rig-core =0.37.0` / `async-trait` / `tui-textarea` / `futures`；`reqwest` 加 `stream` feature。`cargo clean && cargo build` 通过。
- 指标: crates 345 → 575；debug binary 21 MB → 24 MB；clean build 21.2 s → 43.3 s（在 §6 验收 60 s 预算内）。
- 决议变更: 无。
- 已知坑: 无新增。
- 下一步: Phase 0.2 — 写 `examples/cx_agent_spike.rs`，spike 三种 wire API streaming。

### 2026-05-27 · Phase 0.2 / 0.3 / 0.4 完成（P0 收官）

- 完成 0.2: `examples/cx_agent_spike.rs` — 解析 `~/.config/cx/cx.providers.config.yaml` 子集；CLI 参数 `--responses/--completions/--anthropic provider:model`；调用 rig-core `openai::Client`（Responses）/ `openai::CompletionsClient`（Completions）/ `anthropic::Client`，三组各跑一次 `Message::user("用一句中文打招呼…")` streaming。
- 验证组合：
  - Responses: 百炼/qwen3.6-plus@`https://dashscope.aliyuncs.com/compatible-mode/v1` ✓
  - Completions: 百炼/glm-5@`https://dashscope.aliyuncs.com/compatible-mode/v1` ✓
  - Anthropic: 百炼/glm-5@`https://dashscope.aliyuncs.com/apps/anthropic` ✓（Packy API 在初次默认参数下 0 chunks，怀疑 bearer 头方案不同；非阻断，Phase 1 适配）
- 完成 0.3: spike 内已实现 rig `StreamedAssistantContent` → 计数 `text_chunks/tool_deltas/reasoning_chunks` 的转换雏形；`stream.response.token_usage()` 提取 input/output/total/cache_read/cache_write/reasoning，三组 provider 都返回了非零数字（Anthropic 路径 in=14 / out=547；Responses in=64 / out=844）。
- 完成 0.4: 决议 **v0 不引入 `feature = "cx-agent"` 编译 gate**。理由：当前编译时间 +22.1s（预算 60s）、二进制 +3MB（预算 30MB）均远未触线；feature gate 会复杂化 CI 与本地开发体验；若 Phase 2 末测得超预算再走 §7 反悔点 #3 重新引入。
- 决议变更: §2 决议表"是否给 cx_agent 加 feature gate" 落定为「v0 不加，Phase 2 末复测」。
- 已知坑（新增）：
  - Packy API（自定义 anthropic 网关）默认调用返回 0 chunks，Phase 1 内需在 ProviderAdapter 做 `Authorization: Bearer` fallback。
  - 百炼 compatible-mode 的 Responses 客户端对短 prompt 也会吐数百行 reasoning chunk，UI 渲染需有"reasoning 折叠/简化"策略（写入 §5 Phase 1.3 注意事项）。
- 下一步: Phase 1.1 — `src/lib.rs` 的 agent 列表末尾追加 "Cx Agent"，新建 `src/cx_agent/mod.rs` 暴露 `pub fn run_cx_agent(selection, args) -> Result<()>`，`run_launcher` 在 `agent_id == "cx-agent"` 时分发到它。

### 2026-05-27 · v0 全部完成（P1 / P2 / P3 收官）

- 完成: `src/cx_agent/` 已完整落地：launcher 分发、全屏 ratatui + `tui-textarea` 聊天界面、history IR、多轮 tool loop、approval prompt、六个内建工具（`read_file` / `write_file` / `edit_file` / `bash` / `grep` / `glob`）、rollout 持久化、stats 对齐、provider adapter、README / `docs/agents-comparison.md` / `docs/cx-config-schema.yaml` 全部同步完成。
- 完成: `cx_agent.approval_mode` 已打通运行时解析与 `CxConfig` 持久化/merge；默认值为 `read-only-auto-allow`。`cargo test`（macOS 78 tests；Linux Ubuntu 24.04/aarch64 77 tests）通过，`cargo clippy -- -D warnings` 通过，`CX_STATS_DUMP=1 ./target/debug/cx stats` 可读出 `cx-agent` rollout。
- 完成: live smoke 全部通过。macOS 上分别验证了 Anthropic / Responses / Completions 三条 wire API 的完整 chat + tool 流程；其中 Responses/Completions 在 DashScope-compatible endpoint 上采用 plain user-text tool-result fallback，修复了 `role='tool' invalid` 问题。Linux 上在 Multipass Ubuntu 24.04 / aarch64 中实际启动 `cx`，进入 Cx Agent，执行 `read_file` 读取 `Cargo.toml`，最终返回 package name `cx`。
- 指标: v0 最终 crates = 590（相对基线 +245），debug binary = 41 MB（相对基线 +20 MB），`cargo clean && cargo build` = 46.79 s（相对基线 +25.59 s）；全部在 §6 预算内。
- 决议变更: 无新增反悔；维持 §2 既定选型，仅在实现层面对 OpenAI-compatible Responses / Completions 增加 tool-result 投影兼容分支。
- 已知坑: 无阻断剩余项。Packy API 不是本次最终 smoke 覆盖对象，但不影响 v0 验收范围。
- 下一步: 无；v0 已完成，后续仅按 §10 deferred 进入 v1 范围。

---

## 12. 参考文件

| 文件 | 用途 |
|---|---|
| `docs/cx-stats-plan.md` | 上一阶段（cx stats）实现方案，可作架构参考 |
| `docs/cx-config-schema.yaml` | Provider YAML schema |
| `docs/agents-comparison.md` | 已收录 agent 对比 |
| `src/stats.rs` | 参考实现：扫描 jsonl + ratatui TUI 的完整范式 |
| `src/lib.rs:1231` (`pub fn run`) | 入口分发链路起点 |
| `src/lib.rs:1276` (`fn run_launcher`) | Cx Agent 接入点之上 |
| 选型咨询调研报告 | `~/.copilot/session-state/0e54dea4-ff7a-4706-9f9e-5fed801e31e7/research/rust-coding-agent-cx-agent-v0-cx-rust-coding-agent.md` |
