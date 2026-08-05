# cx stats — Token 用量统计 TUI

## 概述

`cx stats` 是一个新增子命令，用于从各 agent 的本地日志中扫描、解析、聚合 token 用量数据，
并以 TUI 形式展示统计面板（类似 Claude Code 的 `/cost` 命令风格）。

数据已天然存在于磁盘上，无需代理拦截或实时监控。

---

## 数据来源

| Agent | 日志路径 | 模型字段 | 用量字段 |
|---|---|---|---|
| **claude** | `~/.claude/projects/{project}/{session}.jsonl` | `message.model` (如 `qwen3.7-max`, `claude-opus-4-7`) | `message.usage.input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens` |
| **codex (CLI)** | `~/.codex/sessions/{date}/rollout-*.jsonl` | `payload.turn_context.model` (如 `gpt-5.4`, `qwen3.6-plus`) | `payload.info.total_token_usage` + `payload.info.last_token_usage`: `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_output_tokens`, `total_tokens` |
| **copilot (Zed内置)** | `~/Library/Application Support/Zed/codex/sessions/{date}/rollout-*.jsonl` | `session_meta.model_provider` (`embedded-zed`) + 需从 session_meta 或 Zed settings 推断实际模型 | 同 codex 结构 |

### 日志文件格式细节

#### Claude agent JSONL

每行一个 JSON 对象。关键字段：

```
type:           "assistant"
message.role:   "assistant"
message.model:  "qwen3.7-max" | "claude-opus-4-7" | ...
message.usage:  { ... }
timestamp:      "2026-05-27T02:36:28.840Z"
sessionId:      UUID
version:        "2.1.150"
entrypoint:     "cli"
cwd:            项目目录路径
```

`message.usage` 结构（Anthropic wire_api 返回）：

```json
{
  "input_tokens":                6,
  "cache_creation_input_tokens": 41090,
  "cache_read_input_tokens":     0,
  "output_tokens":               185,
  "server_tool_use": {
    "web_search_requests": 0,
    "web_fetch_requests": 0
  },
  "service_tier":                "standard",
  "cache_creation": {
    "ephemeral_1h_input_tokens": 0,
    "ephemeral_5m_input_tokens": 41090
  },
  "inference_geo":               "",
  "iterations":                  [],
  "speed":                       "standard"
}
```

注意：早期消息可能只有 `input_tokens` + `output_tokens`，不含 cache 字段。

#### Codex agent JSONL

每行一个 JSON 对象。关键字段：

```
type:               "session_meta" | "event_msg" | "turn_context" | "response_item"
timestamp:          ISO8601
payload.type:       "task_started" | "task_completed" | ...
payload.info.total_token_usage:   累计 token 用量（本 session 内所有 turn 之和）
payload.info.last_token_usage:    最近一次 API 调用的 token 用量
payload.turn_context.model:       "gpt-5.4" | "qwen3.6-plus" | ...
payload.session_meta.model_provider: "openai" | "embedded-zed"
```

`payload.info.total_token_usage` / `last_token_usage` 结构：

```json
{
  "input_tokens":               17723,
  "cached_input_tokens":        4480,
  "output_tokens":              576,
  "reasoning_output_tokens":    303,
  "total_tokens":               18299
}
```

---

## 命令行接口

```bash
cx stats                          # 交互式 TUI 面板
cx stats --period today           # 今天数据
cx stats --period 7d              # 最近7天
cx stats --period 30d             # 最近30天
cx stats --model qwen3.7-max      # 指定模型
cx stats --provider 百炼           # 指定 provider
cx stats --output-format json     # JSON 输出（非 TUI）
cx stats --output-format csv      # CSV 输出
cx stats --output-format markdown # Markdown 表格输出
cx stats --compare 7d 30d         # 比较两个时段
cx stats --top-n 10               # 显示前10模型
cx stats --sort-by-model          # 按模型名排序
cx stats --hide-hist              # 隐藏历史柱状图
cx stats --verbose                # 详细输出
cx stats --version                # 版本信息
```

---

## TUI 面板设计

### 主视图 — 按模型分组汇总

```
┌──────────────────────────────────────────────────────────────┐
│ cx stats · Token Usage Dashboard                    [q] Quit │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│ ── Today · 2026-05-27 ──                                    │
│                                                              │
│  Agent  │ Model            │ Input     │ Output  │ Cached   │
│ ────────│──────────────────│───────────│─────────│────────── │
│  claude │ qwen3.7-max      │ 41,090    │ 185     │ 0        │
│  claude │ claude-opus-4-7  │ 252,341   │ 872     │ 0        │
│  codex  │ gpt-5.4          │ 960K      │ 5.5K    │ 860K     │
│                                                              │
│ ── Last 7 Days ──                                            │
│                                                              │
│  Model            │ Input  │ Output │ Cached │ Cost(¥)       │
│ ──────────────────│───────│───────│───────│──────────────    │
│  qwen3.7-max      │ 1.2M  │ 23.4K │ 800K   │ ¥12.3          │
│  claude-opus-4-7  │ 5.4M  │ 67.2K │ 4.1M   │ ¥186.5         │
│  gpt-5.4          │ 960K  │ 5.5K  │ 860K   │ ¥28.7          │
│  qwen3.6-plus     │ 38K   │ 1.7K  │ 0      │ ¥0.3           │
│                                                              │
│ ── Cost Breakdown ──                                         │
│                                                              │
│  Total estimated cost: ¥228.0                                │
│  Cache savings:     ¥-134.2 (cached tokens billed lower)    │
│                                                              │
│ [↑↓] Scroll  [Tab] Toggle View  [1-4] Switch Period        │
│ [c] Cost View  [t] Token View  [r] Refresh  [q] Quit       │
└──────────────────────────────────────────────────────────────┘
```

### 视图切换

| 键 | 视图 | 说明 |
|---|---|---|
| `1` | Today | 当天数据 |
| `2` | 7d | 最近7天 |
| `3` | 30d | 最近30天 |
| `4` | All | 全部历史 |
| `t` | Token View | 显示 token 数量 |
| `c` | Cost View | 显示估算费用 |
| `r` | Refresh | 重新扫描 |
| `q` | Quit | 退出 |

---

## 数据聚合逻辑

### 核心数据结构

```rust
struct StatsEntry {
    agent: String,        // "claude", "codex", "copilot"
    model: String,        // "qwen3.7-max", "claude-opus-4-7", "gpt-5.4"
    provider: String,     // 从 endpoint_url 或 session 元数据推断
    timestamp: i64,       // Unix timestamp
    session_id: String,   // 会话 ID
    
    // Token counts
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,     // claude: cache_read_input_tokens; codex: cached_input_tokens
    cache_creation_tokens: u64, // claude 专用
    reasoning_tokens: u64,      // codex 专用: reasoning_output_tokens
    total_tokens: u64,          // codex 专用: total_tokens
    
    // Metadata
    entrypoint: String,   // "cli", "vscode", "zed"
    version: String,      // agent 版本号
    cwd: String,          // 工作目录
}
```

### 聚合维度

```
AggregationKey = (agent, model, date)
```

每个 `(agent, model, date)` 组合产生一个汇总行，包含：
- 累计 input_tokens, output_tokens, cache_read_tokens, reasoning_tokens
- API 调用次数（消息条数）
- 估算费用

### Provider 推断

从日志中无法直接获取 cx 的 provider 名称，但可以通过以下方式推断：

1. **claude agent**: 从 `ANTHROPIC_BASE_URL` 环境变量或 session 中的 endpoint 信息推断
   - 如果 cwd 是 cx launch home 路径 → 从 `cx.providers.config.yaml` 反查 provider
   - 否则：从 `message.usage.service_tier` 等辅助信息推断
   
2. **codex agent**: 从 `turn_context.model` 和 `session_meta.model_provider` 推断
   - `model_provider == "embedded-zed"` → Zed 内置 copilot
   - 其他 → 对应 CLI codex

3. **更可靠的方案**: cx 在 `build_launch_spec()` 时，已知 `(provider, model, agent)` 三元组。
   可以在 exec_launch 前写入一个轻量标记文件到 launch home：

```rust
// 在 exec_launch 之前
fn write_selection_marker(selection: &Selection, launch_home: &Path) -> Result<()> {
    let marker = json!({
        "agent": selection.agent_id,
        "provider": selection.provider.name,
        "model": selection.model.map(|m| m.id.clone()),
        "wire_api": selection.model.map(|m| m.wire_api.display()),
        "timestamp": current_unix_secs(),
    });
    write_private_file(launch_home.join("cx-selection.json"), &marker.to_string())?;
    Ok(())
}
```

这样 `cx stats` 可以从 `~/.local/share/cx/launch-homes/*/cx-selection.json` 读取精确的 provider/model 关联，
而不依赖日志推断。

---

## 成本估算

### 定价数据来源

在 `cx.providers.config.yaml` 的 `ProviderModelConfig` 中新增 `pricing` 字段：

```yaml
models:
  qwen3.7-max:
    desc: 当前旗舰最强
    wire_apis:
      - anthropic
      - completions
    agents: []
    pricing:
      input_per_million:     4.0    # ¥/M input tokens
      output_per_million:    16.0   # ¥/M output tokens
      cache_read_per_million: 1.0   # ¥/M cache_read tokens
      cache_creation_per_million: 2.0 # ¥/M cache_creation tokens
```

### 计算公式

```
cost = (input_tokens / 1M) × input_price
     + (output_tokens / 1M) × output_price
     + (cache_read_tokens / 1M) × cache_read_price
     + (cache_creation_tokens / 1M) × cache_creation_price
```

对于未配置 pricing 的模型，显示 `—`（不估算费用）。

### 已知模型定价参考（阿里云百炼）

| 模型 | Input ¥/M | Output ¥/M | Cache Read ¥/M |
|---|---|---|---|
| qwen3.7-max | 4.0 | 16.0 | 1.0 |
| qwen3.6-plus | 0.5 | 2.0 | 0.1 |
| qwen3.5-plus | 0.5 | 2.0 | 0.1 |
| qwen3.5-flash | 免费 | 免费 | — |
| qwen3-max | 2.0 | 6.0 | 0.5 |
| glm-5 | 1.0 | 4.0 | — |
| glm-5.1 | 1.0 | 4.0 | — |
| kimi-k2.5 | 4.0 | 16.0 | 1.0 |
| deepseek-v3.2 | 2.0 | 8.0 | 0.5 |
| deepseek-v4-flash | 1.0 | 4.0 | 0.25 |
| deepseek-v4-pro | 4.0 | 16.0 | 1.0 |
| deepseek-r1 | 4.0 | 16.0 | 1.0 |
| MiniMax-M2.5 | 1.0 | 4.0 | — |
| MiniMax-M2.7 | 1.0 | 8.0 | — |
| qwq-plus | 2.0 | 6.0 | 0.5 |

---

## 实现计划

### 代码结构

在 `cx/src/lib.rs` 中新增以下内容（约 400-500 行 Rust）：

```
新增 enum/struct:
  StatsPeriod        — Today / 7d / 30d / All
  StatsSortBy        — Cost / Tokens / Model
  StatsOutputFormat  — TUI / JSON / CSV / Markdown
  StatsEntry         — 单条用量记录
  AggregatedStats    — 聚合后的统计数据
  StatsAppState      — TUI 状态

新增函数:
  scan_claude_usage()     — 扫描 claude JSONL 日志
  scan_codex_usage()      — 扫描 codex JSONL 日志
  scan_copilot_usage()    — 扫描 Zed copilot JSONL 日志
  aggregate_stats()       — 按 (agent, model, date) 聚合
  estimate_cost()         — 从 config pricing 估算费用
  read_selection_markers() — 从 cx launch homes 读取 provider 标记
  run_stats()             — 入口：根据 output-format 选择 TUI 或文本输出
  run_stats_tui()         — TUI 渲染循环
  render_stats_table()    — 渲染主统计表格
  render_cost_breakdown() — 渲染费用分解面板
```

### 实现步骤

1. **Step 1**: 扩展 `ProviderModelConfig` 增加 `pricing` 字段
2. **Step 2**: 实现 `scan_claude_usage()` — 解析 claude JSONL
3. **Step 3**: 实现 `scan_codex_usage()` — 解析 codex/copilot JSONL
4. **Step 4**: 实现 `aggregate_stats()` + `estimate_cost()`
5. **Step 5**: 实现 `run_stats` CLI 入口 + `--output-format json/csv/markdown` 文本输出
6. **Step 6**: 实现 TUI 面板（依赖 ratatui，cx Cargo.toml 已有）
7. **Step 7**: 在 `exec_launch()` 中写入 selection marker 文件
8. **Step 8**: 用 `read_selection_markers()` 实现精确 provider 推断

### 依赖

- `ratatui` — TUI 渲染（已存在于 cx Cargo.toml）
- `serde_json` — JSON 解析（已存在）
- `serde_yaml` — YAML 配置解析（已存在）
- `chrono` — 时间处理（需新增或用已有 `current_unix_secs()`）
- `glob` — 文件扫描（需新增）

### 测试

- 单元测试：JSONL 解析、聚合逻辑、费用计算
- 集成测试：用 mock 日志目录验证完整流程
- TUI 测试：用 ratatui 的 test backend 验证渲染输出

---

## 风险与注意事项

1. **日志格式变化**: Claude/Codex 版本升级可能改变 JSONL 结构。
   解决方案：解析时做字段兼容性检查，缺失字段填 0。

2. **大日志性能**: 长期使用后日志可能很大（数 GB）。
   解决方案：只解析包含 `usage`/`token_usage` 的行，跳过纯对话内容行；
   提供 `--period` 参数限制扫描范围。

3. **重复计数**: Claude 的 JSONL 中同一消息可能有多条碎片记录（thinking → text → tool_use）
   共享同一个 `message.id` 且 usage 相同。
   解决方案：按 `message.id` 去重，只计一次。

4. **Codex 累计 vs 单次**: `total_token_usage` 是累计值，`last_token_usage` 是增量。
   聚合时应使用 `last_token_usage` 增量值求和，而非直接用 `total_token_usage`（会重复计算）。

5. **模型名称不一致**: 同一模型在不同 agent 中可能有不同命名。
   如 `glm-5` 在百炼叫 `glm-5`，在 Claude wire_api 也叫 `glm-5`，
   但在 OpenAI wire_api 可能叫 `ZHIPU/GLM-5`。
   解决方案：使用 cx selection marker 获取精确的 provider + model 关联。

---

## 与其他 agent 的协作说明

### 对 copilot agent 的要求

- copilot agent 需要支持 `cx stats` 子命令的发现和调用
- copilot agent 可以在对话中主动展示用量摘要

### 对 claude agent 的要求

- claude agent 可以通过 `cx stats --output-format markdown` 获取用量信息
- claude agent 可在会话结束时提示用户查看 `cx stats`

### 对 codex agent 的要求

- codex agent 可通过 `cx stats --output-format json` 获取结构化数据
- codex agent 可将用量数据纳入任务规划（如控制 token 预算）