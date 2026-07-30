# Plan: 以 `pi` crate 替换 manox 自研 harness 内核

## Context

将 Pi coding agent 的 harness 层（agent loop、tool calling、compaction、session 管理）完整移植到 Rust，形成 manox 最终唯一的 harness 内核 `crates/pi`。

`pi` crate 成熟前保持独立开发，不要求 manox 提前接线；这一阶段以尽可能对齐 Pi TS 核心行为、相关 examples 可运行和差分测试通过为验收标准。成熟后，manox 将整体迁移到 `pi` crate，并完全移除目前自研的 harness 内核，不保留长期双栈、旧内核适配层或面向旧实现的兼容承诺。

**范围边界：**
- ✅ 移植：agent loop 状态机、Agent 类、AgentHarness 编排层、compaction、session tree（JSONL 持久化）、7 个内置工具、settings 管理、trust 管理、cache miss 检测
- ❌ 不移植：UI（TUI）、LLM Provider SDK（37 个）、Extension 系统（jiti 动态加载）

**当前规模：** ~6,200 行 Rust，67 个测试，零警告。

## 架构设计

### 文件布局

```
crates/pi/src/
  agent_loop.rs          -- run_loop（纯引擎：双循环状态机、工具调用管道）
  agent.rs               -- Agent（状态管理：steering/follow-up 队列、事件订阅、生命周期）
  harness.rs             -- AgentHarness（编排层：session 持久化、hooks、compaction 集成）
  types.rs               -- 核心类型：AgentMessage、AgentEvent、AgentContext、AgentLoopConfig
  tool.rs                -- AgentTool trait、工具执行管道
  env.rs                 -- ExecutionEnv trait + TokioExecutionEnv 生产实现
  cache_stats.rs         -- Cache miss 检测
  settings.rs            -- Settings 管理（global/project 合并、文件持久化）
  trust.rs               -- Trust 管理
  system_prompt.rs       -- 系统提示词构建（项目上下文 + CLAUDE.md 加载）
  output_guard.rs        -- 工具输出标记（防注入攻击）
  session/
    mod.rs               -- Session、SessionStorage trait
    jsonl.rs             -- JSONL 文件存储实现
  compaction/
    mod.rs               -- Compaction 算法（切点选择、token 估算、摘要生成）
    branch_summarization.rs  -- 分支摘要生成与合并
  tools/
    mod.rs               -- 工具注册表
    read.rs              -- 文件读取（行号格式化、截断）
    write.rs             -- 文件写入（diff 输出）
    edit.rs              -- 文件编辑（search-and-replace + diff-based 模糊匹配）
    edit_diff.rs         -- 统一 diff 计算（similar crate）
    bash.rs              -- Shell 命令执行（输出截断）
    grep.rs              -- 内容搜索（进程内：ignore + regex + globset）
    find.rs              -- 文件搜索（进程内：ignore + globset）
    ls.rs                -- 目录列表（人类可读大小、截断）
    file_mutation_queue.rs  -- 同文件并发写入串行化（正确性设施）
    truncate.rs          -- 输出截断（按行 + 按字节，保留 head+tail）
    output_accumulator.rs   -- 大输出溢写到临时文件
    path_utils.rs        -- 路径解析、~ 展开、安全边界检查
```

### 关键设计决策

| 决策点 | 选择 | 理由 |
|--------|------|------|
| 异步运行时 | tokio | manox 已使用 tokio |
| 错误处理 | thiserror（领域错误）+ anyhow（胶水代码） | 匹配 manox 现有模式 |
| 事件系统 | `tokio::mpsc::Sender<AgentEvent>` | 比 callback trait 更 Rust 惯用，天然支持背压 |
| StreamFn | `Arc<dyn StreamFn>` + mpsc channel | Arc 为 tokio::spawn 提供 'static lifetime |
| 生产 ExecutionEnv | `TokioExecutionEnv`（tokio::fs + tokio::process::Command） | 真实文件系统 + shell，超时通过 tokio::time::timeout |
| grep/find | 进程内实现（ignore + regex + globset） | 消除 shell 注入，不依赖系统 grep/find |
| edit 工具 | `similar` crate 做 diff-based 模糊匹配 | 处理 LLM 缩进/空白漂移 |
| edit_diff | `similar` crate 计算统一 diff | 编辑后返回 diff 展示变更 |
| 输出截断 | `truncate.rs` 共享函数，所有工具统一调用 | 避免上下文窗口溢出 |
| Message 类型 | enum + `#[non_exhaustive]` Custom 变体 | 替代 TS declaration merging |
| Session 持久化 | JSONL（追加写入） | 与 Pi 一致，比 SQLite 简单，不需要 migration |
| ExecutionEnv | trait（合并 FileSystem + Shell） | 运行时无关，可注入测试实现 |
| 工具参数验证 | `jsonschema`（workspace dep） | 运行时校验 LLM 工具调用参数 |

### 关键 trait 签名

```rust
// LLM 流式响应 —— 通过 mpsc channel 发送事件
#[async_trait]
pub trait StreamFn: Send + Sync {
    async fn stream(
        &self,
        context: &AgentContext,
        signal: CancellationToken,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error>;
}

// 平台抽象——文件系统 + Shell
#[async_trait]
pub trait ExecutionEnv: Send + Sync {
    fn cwd(&self) -> &Path;
    async fn read_file(&self, path: &Path, offset: Option<usize>, limit: Option<usize>) -> Result<String, FileError>;
    async fn write_file(&self, path: &Path, content: &str) -> Result<(), FileError>;
    async fn exec(&self, command: &str, timeout: Duration) -> Result<CommandResult, ExecutionError>;
    // ... 其他方法
}

// 工具 trait
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    fn is_read_only(&self) -> bool;
    fn requires_approval(&self, params: &Value) -> bool;
    async fn execute(
        &self, tool_call_id: &str, params: Value,
        signal: CancellationToken, ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError>;
}
```

## 实施步骤

### Phase 1: 基础类型 + 抽象边界 ✅

**文件：** `types.rs`, `env.rs`, `tool.rs`

1. ✅ 定义 `AgentMessage` enum（User/Assistant/ToolResult/Custom）
2. ✅ 定义 `AgentEvent` enum（完整生命周期事件）
3. ✅ 定义 `AgentContext` struct（messages + tools + system_prompt）
4. ✅ 定义 `AgentLoopConfig` struct（hooks as callback fns）
5. ✅ 定义 `ExecutionEnv` trait
6. ✅ 定义 `AgentTool` trait + `AgentToolResult` struct
7. ✅ 实现工具执行管道（prepare → validate → before_hook → execute → after_hook → finalize）

### Phase 2: 核心引擎 ✅

**文件：** `agent_loop.rs`, `agent.rs`

1. ✅ 实现 `run_loop()` —— 双循环状态机
   - 外层：follow-up 消息循环
   - 内层：tool call + steering 循环
   - `stream_fn` 调用 → 提取 tool calls → 执行工具 → 注入结果 → 循环
   - 截断安全：stop_reason == "length" 时 fail 所有 tool calls
   - 中止检查：每个 tool call 边界检查 CancellationToken
2. ✅ 实现 `Agent` struct
   - 状态管理（MutableAgentState）
   - 事件订阅（`subscribe()`）
   - 队列管理（steering + follow-up + next_turn）
   - 生命周期（prompt / continue / abort / reset / wait_for_idle）

### Phase 3: Session + Compaction ✅

**文件：** `session/mod.rs`, `session/jsonl.rs`, `compaction/`

1. ✅ 定义 `SessionTreeEntry` enum（所有 entry 类型）
2. ✅ 定义 `SessionStorage` trait + `SessionRepo` trait
3. ✅ 实现 `JsonlSessionStorage`（追加写入 JSONL，leaf 游标管理）
4. ✅ 实现 `Session` struct（context 构建、entry 追加、tree navigation）
5. ✅ 实现 `find_cut_point()` —— 从尾向头遍历找安全切点
6. ✅ 实现 `estimate_tokens()` —— 字符数/4 + provider usage
7. ✅ 实现 `prepare_compaction()` + `compact()` —— 调用 stream_fn 生成摘要

### Phase 4: AgentHarness 编排层 ✅

**文件：** `harness.rs`

1. ✅ 实现 `AgentHarness` struct
   - Phase 状态机（idle / turn / compaction / branch_summary / retry）
   - 并发保护：结构化操作只能在 idle 时执行
2. ✅ 实现 hook 系统
   - `on()` 注册 hook handler
   - hook 点：before_agent_start, context, before_provider_request, tool_call, tool_result, session_before_compact 等
3. ✅ 实现 compaction 集成 —— `compact()` 方法编排完整流程
4. ✅ 实现 turn state 快照 —— 每次 turn 开始前快照 context
5. ✅ 实现 session 写入缓冲 —— 活跃 turn 期间缓冲写入，turn 边界刷新

### Phase 5: 内置工具 ✅

**文件：** `tools/read.rs`, `write.rs`, `edit.rs`, `bash.rs`, `grep.rs`, `find.rs`, `ls.rs`

1. ✅ 定义 JSON Schema（serde_json::json!）
2. ✅ 实现 `AgentTool` trait
3. ✅ 通过 `ExecutionEnv` trait 访问文件系统和 Shell
4. ✅ 输出截断（DEFAULT_MAX_BYTES / DEFAULT_MAX_LINES）—— 所有工具统一调用 `truncate()`
5. ✅ 错误处理（文件不存在、权限不足、超时等）

额外实现的工具层模块：
- ✅ `edit_diff.rs` —— 统一 diff 计算，edit/write 工具返回 diff 展示变更
- ✅ `file_mutation_queue.rs` —— 同文件并发写入串行化
- ✅ `truncate.rs` —— 共享输出截断，保留 head+tail
- ✅ `output_accumulator.rs` —— 大输出溢写到临时文件
- ✅ `path_utils.rs` —— 路径解析、安全边界检查

### Phase 6: 辅助模块 ✅

**文件：** `settings.rs`, `trust.rs`, `cache_stats.rs`, `system_prompt.rs`, `output_guard.rs`

1. ✅ `settings.rs` —— Settings struct + global/project 合并 + JSON 文件持久化
2. ✅ `trust.rs` —— 项目信任决策管理
3. ✅ `cache_stats.rs` —— 逐 turn 扫描 usage 字段，检测 cache miss 和浪费金额
4. ✅ `system_prompt.rs` —— 系统提示词构建（项目上下文 + CLAUDE.md 加载）
5. ✅ `output_guard.rs` —— 工具输出标记（防注入攻击）

### Phase 7: 真实执行环境 + 测试 ✅

1. ✅ `TokioExecutionEnv` —— tokio::fs + tokio::process::Command 生产实现
2. ✅ grep/find 进程内实现（ignore + regex + globset），消除 shell 注入
3. ✅ 截断接入所有输出工具（read/write/bash/ls/grep/find）
4. ✅ agent_loop 核心测试：单轮、多轮 tool call、截断安全、abort、follow-up
5. ✅ edit_diff 单元测试（统一 diff 计算、空 diff、hunk 计数）
6. ✅ 67 个单元测试全通过，零警告

## 待完成

- [ ] `pi` crate 加入 workspace members（需在 manox 根 Cargo.toml 添加 `crates/pi`）
- [ ] 集成测试（`crates/pi/tests/` 目录，真实 tokio 环境）
- [ ] 与 Pi TS 的差分测试（相同输入 → 相同输出）
- [ ] `OutputAccumulator` 接入流式 exec（需 ExecutionEnv 支持流式输出）
- [ ] `schemars` 生成工具 JSON Schema（从 Rust struct 派生，替代手写 serde_json::json!）

## 与 manox 现有 `agent` crate 的关系

`pi` crate 是 manox harness 内核的**最终替代实现**，不是与现有自研 harness 长期共存的可选库。迁移分为两个阶段：

1. **独立成熟阶段**
   - `pi` crate 独立实现完整的 agent harness（loop + harness + compaction + session + tools）
   - 暂不要求 manox 接线，避免未成熟内核影响现有功能
   - 以 Pi TS 核心语义为基准持续补齐实现和差分测试

2. **整体迁移阶段**
   - manox 完全切换到 `pi` crate 作为 harness 内核
   - 删除 manox 当前自研 harness 内核及其重复实现
   - 不保留新旧内核并行运行、按组件选择接入或旧 harness API 兼容层

迁移期间可以编写一次性的调用侧改造代码，但这些代码只服务于完成切换，不构成长期兼容边界。迁移完成后的架构中只保留 `pi` crate 这一套 harness 内核。

## 交付方式

- 使用 `/deliver`（gitwork:deliver）创建 branch、实现变更、推送并创建 PR
- 计划原文写入 `crates/pi/PLAN.md`

## 验证计划

1. ✅ `cargo build -p pi` 编译通过
2. ✅ `cargo test -p pi` 所有测试通过（67 个，零警告）
3. ✅ 用 mock `StreamFn` 运行完整 agent loop，验证双循环状态机行为（5 个测试覆盖）
4. ✅ 用临时目录测试 JSONL session 持久化（4 个测试覆盖）
5. ✅ 用 mock `ExecutionEnv` 测试工具执行管道
6. ✅ 测试 compaction 切点选择和摘要生成（6 个测试覆盖）
7. ⬜ 差分测试：相同输入下 Pi TS 与 Pi Rust 输出一致性
8. ⬜ 集成测试：`TokioExecutionEnv` + 真实文件系统 + 多轮 agent loop
