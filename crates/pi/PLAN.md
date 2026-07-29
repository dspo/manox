# Plan: 在 manox 中增加 `pi` crate

## Context

将 Pi coding agent 的 harness 层（agent loop、tool calling、compaction、session 管理）移植到 Rust，作为 manox 工作区的一个新 crate `crates/pi`。

**范围边界：**
- ✅ 移植：agent loop 状态机、Agent 类、AgentHarness 编排层、compaction、session tree（JSONL 持久化）、7 个内置工具、settings 管理、trust 管理、cache miss 检测
- ❌ 不移植：UI（TUI）、LLM Provider SDK（37 个）、Extension 系统（jiti 动态加载）

**输入规模：** ~9,700 行 TypeScript → 预计 ~12,000-15,000 行 Rust

## 架构设计

### 三层结构（与 Pi 对应）

```
crates/pi/src/
  harness.rs          -- AgentHarness（编排层：session 持久化、hooks、compaction 集成）
  agent.rs            -- Agent（状态管理：steering/follow-up 队列、事件订阅、生命周期）
  loop.rs             -- run_loop（纯引擎：双循环状态机、工具调用管道）
  types.rs            -- 核心类型：AgentMessage、AgentEvent、AgentContext、AgentLoopConfig
  tool.rs             -- AgentTool trait、工具执行管道
  compaction.rs       -- Compaction 算法（切点选择、token 估算、摘要生成）
  cache_stats.rs      -- Cache miss 检测
  settings.rs         -- Settings 管理（global/project 合并、文件持久化）
  trust.rs            -- Trust 管理
  env.rs              -- ExecutionEnv trait（FileSystem + Shell 抽象）
  session/
    mod.rs            -- Session、SessionStorage trait、SessionRepo trait
    jsonl.rs          -- JSONL 文件存储实现
  tools/
    mod.rs            -- 工具注册表
    read.rs           -- 文件读取
    write.rs          -- 文件写入
    edit.rs           -- 文件编辑（search-and-replace）
    bash.rs           -- Shell 命令执行
    grep.rs           -- 内容搜索
    find.rs           -- 文件搜索
    ls.rs             -- 目录列表
```

### 关键设计决策

| 决策点 | 选择 | 理由 |
|--------|------|------|
| 异步运行时 | tokio | manox 已使用 tokio |
| 错误处理 | thiserror（领域错误）+ anyhow（胶水代码） | 匹配 manox 现有模式 |
| 事件系统 | `tokio::mpsc::Sender<AgentEvent>` | 比 callback trait 更 Rust 惯用，天然支持背压 |
| StreamFn | `async fn` 返回 `Pin<Box<dyn Stream>>` | 最干净的边界，不依赖具体 provider |
| Message 类型 | enum + `#[non_exhaustive]` Custom 变体 | 替代 TS declaration merging |
| Session 持久化 | JSONL（追加写入） | 与 Pi 一致，比 SQLite 简单，不需要 migration |
| ExecutionEnv | trait（合并 FileSystem + Shell） | 运行时无关，可注入测试实现 |
| 工具参数验证 | `schemars` + `jsonschema` | 与 Pi 的 TypeBox 对应 |

### 关键 trait 签名

```rust
// 唯一的"外部"依赖——LLM 流式响应
#[async_trait]
pub trait StreamFn: Send + Sync {
    async fn stream(
        &self,
        model: &Model,
        context: &AgentContext,
        options: &StreamOptions,
        signal: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<AgentEvent>> + Send>>, Error>;
}

// 平台抽象——文件系统 + Shell
#[async_trait]
pub trait ExecutionEnv: Send + Sync {
    async fn read_file(&self, path: &Path, offset: usize, limit: usize) -> Result<String>;
    async fn write_file(&self, path: &Path, content: &str) -> Result<()>;
    async fn exec(&self, command: &str, timeout: Duration) -> Result<CommandResult>;
    // ... 其他方法
}

// 工具 trait
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    fn execution_mode(&self) -> ExecutionMode;
    async fn execute(
        &self, tool_call_id: &str, params: Value,
        signal: CancellationToken, ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult>;
}
```

## 实施步骤

### Phase 1: 基础类型 + 抽象边界（1-2 天）

**文件：** `types.rs`, `env.rs`, `tool.rs`

1. 定义 `AgentMessage` enum（User/Assistant/ToolResult/Custom）
2. 定义 `AgentEvent` enum（完整生命周期事件）
3. 定义 `AgentContext` struct（messages + tools + system_prompt）
4. 定义 `AgentLoopConfig` struct（hooks as callback fns）
5. 定义 `ExecutionEnv` trait
6. 定义 `AgentTool` trait + `AgentToolResult` struct
7. 实现工具执行管道（prepare → validate → before_hook → execute → after_hook → finalize）

### Phase 2: 核心引擎（1-2 天）

**文件：** `loop.rs`, `agent.rs`

1. 实现 `run_loop()` —— 双循环状态机
   - 外层：follow-up 消息循环
   - 内层：tool call + steering 循环
   - `stream_fn` 调用 → 提取 tool calls → 执行工具 → 注入结果 → 循环
   - 截断安全：stop_reason == "length" 时 fail 所有 tool calls
   - 中止检查：每个 tool call 边界检查 CancellationToken
2. 实现 `Agent` struct
   - 状态管理（MutableAgentState）
   - 事件订阅（`subscribe()`）
   - 队列管理（steering + follow-up + next_turn）
   - 生命周期（prompt / continue / abort / reset / wait_for_idle）

### Phase 3: Session + Compaction（1-2 天）

**文件：** `session/mod.rs`, `session/jsonl.rs`, `compaction.rs`

1. 定义 `SessionTreeEntry` enum（所有 entry 类型）
2. 定义 `SessionStorage` trait + `SessionRepo` trait
3. 实现 `JsonlSessionStorage`（追加写入 JSONL，leaf 游标管理）
4. 实现 `Session` struct（context 构建、entry 追加、tree navigation）
5. 实现 `find_cut_point()` —— 从尾向头遍历找安全切点
6. 实现 `estimate_tokens()` —— 字符数/4 + provider usage
7. 实现 `prepare_compaction()` + `compact()` —— 调用 stream_fn 生成摘要

### Phase 4: AgentHarness 编排层（1-2 天）

**文件：** `harness.rs`

1. 实现 `AgentHarness` struct
   - Phase 状态机（idle / turn / compaction / branch_summary / retry）
   - 并发保护：结构化操作只能在 idle 时执行
2. 实现 hook 系统
   - `on()` 注册 hook handler
   - hook 点：before_agent_start, context, before_provider_request, tool_call, tool_result, session_before_compact 等
3. 实现 compaction 集成 —— `compact()` 方法编排完整流程
4. 实现 turn state 快照 —— 每次 turn 开始前快照 context
5. 实现 session 写入缓冲 —— 活跃 turn 期间缓冲写入，turn 边界刷新

### Phase 5: 内置工具（1-2 天）

**文件：** `tools/read.rs`, `write.rs`, `edit.rs`, `bash.rs`, `grep.rs`, `find.rs`, `ls.rs`

每个工具：
1. 定义 JSON Schema（schemars）
2. 实现 `AgentTool` trait
3. 通过 `ExecutionEnv` trait 访问文件系统和 Shell
4. 输出截断（DEFAULT_MAX_BYTES / DEFAULT_MAX_LINES）
5. 错误处理（文件不存在、权限不足、超时等）

### Phase 6: 辅助模块（0.5-1 天）

**文件：** `settings.rs`, `trust.rs`, `cache_stats.rs`

1. `settings.rs` —— Settings struct + global/project 合并 + JSON 文件持久化
2. `trust.rs` —— 项目信任决策管理
3. `cache_stats.rs` —— 逐 turn 扫描 usage 字段，检测 cache miss 和浪费金额

### Phase 7: 集成 + 测试（1-2 天）

1. 编写单元测试（mock ExecutionEnv + mock StreamFn）
2. 编写集成测试（完整 agent loop 运行）
3. 在 manox 的 `Cargo.toml` 中注册 `pi` crate
4. 验证与 manox 现有 `agent` crate 的共存关系

## 与 manox 现有 `agent` crate 的关系

Pi crate 是**独立库**，不修改 manox 现有 `agent` crate。两者的关系：

- Pi crate 提供完整的 agent harness（loop + harness + compaction + session + tools）
- manox 的 `agent` crate 可以**选择使用** Pi crate 的组件（如 compaction 算法、tool 实现）
- 如果未来统一，可以在 manox `agent` 中实现 `ExecutionEnv` trait 并注入 Pi harness

## 交付方式

- 使用 `/deliver`（gitwork:deliver）创建 branch、实现变更、推送并创建 PR
- 计划原文写入 `crates/pi/PLAN.md`

## 验证计划

1. `cargo build -p pi` 编译通过
2. `cargo test -p pi` 所有测试通过
3. 用 mock `StreamFn` 运行完整 agent loop，验证双循环状态机行为
4. 用临时目录测试 JSONL session 持久化
5. 用 mock `ExecutionEnv` 测试每个内置工具
6. 测试 compaction 切点选择和摘要生成