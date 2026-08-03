# Plan: 以 `pi` crate 替换 manox 自研 harness 内核

## Context

将 Pi coding agent 的 harness 层（agent loop、tool calling、compaction、session 管理）完整移植到 Rust，形成 manox 最终唯一的 harness 内核 `crates/pi`。

`pi` crate 成熟前保持独立开发，不要求 manox 提前接线；这一阶段以尽可能对齐 Pi TS 核心行为、相关 examples 可运行和差分测试通过为验收标准。成熟后，manox 将整体迁移到 `pi` crate，并完全移除目前自研的 harness 内核，不保留长期双栈、旧内核适配层或面向旧实现的兼容承诺。

**范围边界：**
- ✅ 移植：agent loop 状态机、Agent 类、AgentHarness 编排层、compaction、session tree（JSONL 持久化）、7 个内置工具、settings 管理、trust 管理、cache miss 检测
- ❌ 不移植：UI（TUI）、LLM Provider SDK（37 个）、Extension 系统（jiti 动态加载）

**当前规模：** ~27,500 行 Rust（src 25.2k + examples 2.3k），427 个 unit 测试 + 12 个 integration 测试（另 3 个 live 测试 ignored），零警告。

**基线（S0 冻结）：** Rust HEAD `b9b6869`（工作区含未提交 S0 校准：10 改 2 增）；TS Pi `4488ad55c18f07ae89a489096c90de8667b3adfb`（与 `origin/main` 一致）。S0 完成后冻结共享基线，其余 agent 在此基线上开始。

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
  settings.rs            -- Settings 管理（global/project 合并、serde 序列化助手）
  trust.rs               -- Trust 管理（内存态）
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
    edit.rs              -- 文件编辑（hashline 锚定补丁：行号 + TAG 校验，stale TAG 时 3-way merge 恢复）
    edit_diff.rs         -- 统一 diff 计算（similar crate）
    bash.rs              -- Shell 命令执行（输出截断）
    grep.rs              -- 内容搜索（进程内：ignore + regex + globset）
    find.rs              -- 文件搜索（进程内：ignore + globset）
    ls.rs                -- 目录列表（人类可读大小、截断）
    file_mutation_queue.rs  -- 同文件并发写入串行化（正确性设施）
    truncate.rs          -- 输出截断（按行 + 按字节，保留 head+tail）
    path_utils.rs        -- 路径解析、~ 展开、安全边界检查
```

### 关键设计决策

| 决策点 | 选择 | 理由 |
|--------|------|------|
| 异步运行时 | tokio | manox 已使用 tokio |
| 错误处理 | thiserror（领域错误）+ anyhow（胶水代码） | 匹配 manox 现有模式 |
| 事件系统 | `#[async_trait] EventSink` + `mpsc` 有界通道（容量 64） | 发送方 await 每次发射，慢消费者直接背压发射者；对齐 TS Pi 每次 `await emit(...)` 的顺序保证 |
| StreamFn | `Arc<dyn StreamFn>` + mpsc channel | Arc 为 tokio::spawn 提供 'static lifetime |
| 生产 ExecutionEnv | `TokioExecutionEnv`（tokio::fs + tokio::process::Command） | 真实文件系统 + shell；exec 带 CancellationToken——独占进程组（process_group(0)），超时或取消时 SIGKILL 整个进程树（对齐 TS killProcessTree 的负 pid 组杀），stdout/stderr 并发排空防管道死锁 |
| grep/find | 进程内实现（ignore + regex + globset） | 消除 shell 注入，不依赖系统 grep/find |
| edit 工具 | hashline 锚定补丁（行号 + TAG 校验，stale TAG 时 3-way merge 恢复） | 编辑以 read 输出的行号为锚，规避 search-and-replace 的歧义匹配 |
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
   - 工具 progress 实时转发：同步 `emit` 经 unbounded channel 由并发转发 future 排空到 sink（TS `updateEvents` 同序保证——全部 update 先于 `ToolExecutionEnd` 结算，且消费者实时可见而非执行结束后回放）

### Phase 2: 核心引擎 ✅

**文件：** `agent_loop.rs`, `agent.rs`

1. ✅ 实现 `run_loop()` —— 双循环状态机
   - 外层：follow-up 消息循环
   - 内层：tool call + steering 循环
   - `stream_fn` 调用 → 提取 tool calls → 执行工具 → 注入结果 → 循环
   - 截断安全：stop_reason == "length" 时 fail 所有 tool calls
   - 中止检查：每个 tool call 边界检查 CancellationToken
2. ✅ 实现 `Agent` struct
   - 状态归约（`process_event` 对齐 TS `processEvents`：transcript 仅经 `MessageEnd` 增长，`streaming_message`/`pending_tool_calls`/`error_message` 随事件更新）
   - 事件订阅（`subscribe()` → `Subscription`，监听器按注册序 await，`agent_end` 监听器结算后才算 idle）
   - `MessageUpdate` 携带 `AssistantMessageEvent`（9 个流中变体：text/thinking/toolcall 的 start/delta/end），三 provider 形状各自映射
   - 队列管理（steering + follow-up）
   - 生命周期（prompt / continue / abort / reset / wait_for_idle）
   - 已知偏离：Completions 形状块模型与 TS 不同——TS 每条流只合并一个 textBlock/thinkingBlock（交错并入同一块），crate pi 交错时关闭当前块另开新块，终态 content 形状不同（后续 triage）

### Phase 3: Session + Compaction ✅

**文件：** `session/mod.rs`, `session/jsonl.rs`, `compaction/`

1. ✅ 定义 `SessionTreeEntry` enum（所有 entry 类型，v3 schema 与 TS 逐字段对齐——CompactionEntry 含可选 `retainedTail`）
2. ✅ 定义 `SessionStorage` trait
3. ✅ 实现 `JsonlSessionStorage`（追加写入 JSONL，leaf 游标管理）
4. ✅ 实现 `Session` struct（context 构建、typed entry 追加）
   - `get_path`：全路径 walk（leaf→root 跨压缩边界；显式未知 leaf 或断链 parent 显式报错，`None`→空路径，对齐 TS agent 包 jsonl-storage 的 `not_found`/`invalid_session`）
   - `get_branch` / `build_context_entries` / `build_session_context`：对齐 TS `getBranch`/`buildContextEntries`/`buildSessionContext`——最新压缩边界领衔 + `retainedTail` 存在时直接投影、缺失时从 `first_kept_entry_id` 走树重建 kept 段；全变体投影（CustomMessage/BranchSummary/Compaction 各归其位），设置类 entry（thinking_level/model_change/active_tools_change/assistant 见证）经 `SessionContext` 上报
   - 摘要载体统一走 TS tag 常量（`COMPACTION_SUMMARY_*`/`BRANCH_SUMMARY_*`），压缩写入与恢复投影同形
5. ✅ 实现 `find_cut_point()` —— 从尾向头遍历找安全切点
6. ✅ 实现 `estimate_tokens()` —— 字符数/4 + provider usage
7. ✅ 实现 `prepare_compaction()` + `compact()` —— 调用 stream_fn 生成摘要
   - 摘要请求逐字对齐 TS `generateSummaryWithUsage`：`serialize_conversation`（user/custom 文本、assistant thinking/文本/tool calls（name+k=JSON 参数）、tool result 截 2000 字符）、`SUMMARIZATION_SYSTEM_PROMPT` 常量、`SUMMARIZATION_PROMPT`/`UPDATE_SUMMARIZATION_PROMPT` 双模板经 `<previous-summary>` 切换
   - 无可摘要范围时拒绝（`NothingToCompact`，对齐 TS `prepareCompaction` 返回 `undefined`）：tiny transcript 在默认 keep-recent 下切点恒为 0，不再产出空摘要
   - split-turn（turn prefix 摘要，cut 恒在整轮边界）已由 Phase 3A 闭环

### Phase 4: AgentHarness 编排层 ✅

**文件：** `harness.rs`

1. ✅ 实现 `AgentHarness` struct
   - Phase 状态机（idle / turn / compaction / branch_summary / retry）
   - 并发保护：结构化操作只能在 idle 时执行
2. ✅ 实现 hook 系统
   - `on()` 注册 hook handler
   - hook 点：before_agent_start（结果生效：messages 注入进 prompt 批次、systemPrompt 覆盖只达本 run 初始 context）, before_provider_request（逐 provider 调用变换整个 context，覆盖 TS `context` transform 接缝）, tool_call（block）, tool_result（全字段 patch 含 terminate）, session_before_compact（cancel/全量 override）, session_after_compact
   - 有意推迟（无 fire 点/接缝）：payload/response、tree、retry、update 通知类变体——见 docs/ts-pi-parity.md §9「有意偏离」
   - 取消传播到执行层：`ExecutionEnv::exec` 带 CancellationToken，Tokio 实现独占进程组（`process_group(0)`），取消/超时 SIGKILL 整个进程树（对齐 TS `killProcessTree`），bash 工具透传 signal
3. ✅ 实现 compaction 集成 —— `compact()` 方法编排完整流程
4. ✅ 实现 turn state 快照 —— 每次 turn 开始前快照 context
5. ✅ 实现 session 持久化 —— harness persistence middleware 在每条 `MessageEnd` 立即 append（先于 listener），删除 turn 末批量写入；mutation 在下一 provider request 前 flush（prepare-next-turn）；append 失败时 run 终止并回滚 transcript 到 session 前缀（2026-08-01 Phase 4B）
6. ✅ 实现 overflow → compact → retry 闭环
   - assistant 错误消息经 `is_context_overflow` 三判据分类（错误消息模式 / Stop 但 input+cacheRead 超窗 / Length 且 output=0 且 input ≥99% 窗）
   - 同模型守卫、stale 守卫（错误不晚于最近一次压缩）、aborted 不恢复
   - 一次性预算（TS `_overflowRecoveryAttempted`）：失败终端从 transcript 摘除（session 保留）→ 压缩 → `continue_()` 重试一次；新 user prompt 或任何非错误 assistant 消息重新武装
7. ✅ 实现 threshold compact-no-retry 与压缩期队列安全
   - settled 回合（成功/错误）后计量超阈值即为下一回合压缩、不重试（对齐 TS `_checkCompaction` 第二路径）；维护性压缩失败只记日志，不拖垮已完结回合
   - 压缩只替换 transcript：`Agent::clear_transcript_state` 与全清队列的 `reset` 分离，压缩/restore 走前者——steering/follow-up 队列在压缩窗口不丢消息；压缩后队列非空则续跑一次 drain continuation 投递
8. ✅ 运行配置 API 与 restore 回放
   - `set_model` / `set_active_tools` / `set_thinking_level`：应用并持久化 `model_change` / `active_tools_change` / `thinking_level_change` entry（未知工具名拒绝）
   - 运行中 mutation：`HarnessHandle::set_model/set_thinking_level` 立即更新共享 TurnRuntime 快照，prepare-next-turn 在下一轮 provider 请求前刷新 context；持久化队列逐条成功才 pop，失败项留待下次 flush；`with_stream_resolver` 按 `Model.api` 每次请求解析 provider runtime（consumer 插拔，crate registry-free）
   - `restore()` 回放 path 携带的完整运行配置：thinking tier、active tools 子集（经全量挂载集过滤）、model（经 consumer 插接的 `ModelResolver`——crate 保持 registry-free，无 resolver 时保留构造期 model）；restore 不追加任何 entry

> 能力校准：以上 Phase 只覆盖模块基线（文件存在、主路径可用）。TS Pi 行为逐项对齐状态以 `docs/ts-pi-parity.md` 为准——split-turn、message_end 逐条持久化、运行中 active_tools 排队均已闭环（2026-08-01）；Session store/reader/repository 深度（upstream 4488ad55c 之后）、coding-agent facade、hook 通知类变体（见 ts-pi-parity §9）等仍属未完成能力，Phase 的 ✅ 不等于完整迁移。

### Phase 4A：运行配置闭环 ✅（2026-08-01）

- `Model` 全字段判等（provider/api/id/context_window/max_tokens/thinking/metadata）
- `restore()` 同步 Agent、Harness、共享 TurnRuntime 快照三态（model/thinking）
- `StreamResolver` 失败生成 terminal `Assistant(Error)`，正常发出 MessageEnd/TurnEnd/AgentEnd（loop 层不再 `?` 传播）
- 无 resolver 时跨 API 切换显式报错（idle `set_model` 返回 Err；`HarnessHandle::set_model` 拒绝）；`harness_chat` api 修正为 `anthropic`
- 回归：restore_then_prompt / same_id×api / same_id×provider / resolver_failure×3；example `runtime_switch`（A tool-use → 切 B → B 完成 → reopen → 仍 B）

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
- ✅ `path_utils.rs` —— 路径解析、安全边界检查

### Phase 6: 辅助模块 🟡（类型就绪，接线见 S5）

**文件：** `settings.rs`, `trust.rs`, `cache_stats.rs`, `system_prompt.rs`, `output_guard.rs`

1. 🟡 `settings.rs` —— Settings struct + field-wise 合并（serde 序列化助手）；文件加载/保存/reload/递归覆盖未接线（接线方待 S5）
2. 🟡 `trust.rs` —— 项目信任决策管理（内存态，无持久化、未接入资源/工具启用策略）
3. 🟡 `cache_stats.rs` —— 逐 turn 扫描 usage 字段检测 cache miss（token 维度：missed_tokens + miss 计数，带 noise floor）；金额与 idle 未实现（`missed_cost` 恒 0、`idle_ms` 占位、`ModelPriceSource` 未接线——见 S5）
4. ✅ `system_prompt.rs` —— 系统提示词构建（项目上下文 + CLAUDE.md 加载）
5. 🚫 `output_guard.rs` —— 默认关闭：未接入任何工具输出路径，无可观察行为；维持默认关闭，除非差分证明不改变 TS 模型可见行为

### Phase 7: 真实执行环境 + 测试 ✅

1. ✅ `TokioExecutionEnv` —— tokio::fs + tokio::process::Command 生产实现
2. ✅ grep/find 进程内实现（ignore + regex + globset），消除 shell 注入
3. ✅ 截断接入所有输出工具（read/write/bash/ls/grep/find）
4. ✅ agent_loop 核心测试：单轮、多轮 tool call、截断安全、abort、follow-up
5. ✅ edit_diff 单元测试（统一 diff 计算、空 diff、hunk 计数）
6. ✅ 单元测试全通过，零警告（计数见文首「当前规模」，随实现滚动更新）

### Phase 3A：split-turn compaction ✅（2026-08-01）

- `find_cut_point_split`：cut 落在轮内时返回 turn start；turn prefix 单独摘要（`TURN_PREFIX_SUMMARIZATION_PROMPT`），history + prefix 双调用合并 text/usage
- pre-prompt compaction：aborted 回合后 `prompt()` 前执行
- example `split_turn_compact`：90k→179 tokens，reopen 一致

### Phase 8：coding-agent facade 🟡（类型骨架，纵向闭环见 S6）

- `coding_agent` 模块：AgentSession/Builder、ModelRuntime（env credential，缺凭证生成 `missing-*_API_KEY` 假值——S3 改 typed missing-credential 错误）、ResourceLoader（CLAUDE.md/skills/templates 有类型未全接线）、`create_agent_session`
- `open()` 不自动 restore（调用者需手动恢复，S6 改为返回前 restore）；无 `fork`/事件订阅面/shutdown（S6/S4）
- example `coding_agent_smoke`：资源加载→工具轮→model 切换→compact→close/reopen→continue

### 待完成

TS Pi 对齐的已知余项（逐项对齐核验见 `docs/ts-pi-parity.md`，该文件为准）：

- [ ] cache_stats 金额与 idle：`missed_cost` 恒 0、`idle_ms` 占位，需接线 `ModelPriceSource` 与消息时间戳（S5）
- [ ] summarization retry：branch summary 与 compaction 的 summarization 调用无 retry 策略（abort/取消通道已由 S1 接入）
- [x] branch summarization 输入/提示词/结果：按 TS `getMessageFromEntry`/`prepareBranchEntries` 重写，删除自创 `render_messages`/300 字 prompt（S1，2026-08-01）；navigate label/abort/hook 同步对齐
- [ ] Hook 推迟项：payload/response、tree、retry、update 通知类变体（见 ts-pi-parity §9「有意偏离」）
- [ ] Session store/reader/repository 深度（upstream 4488ad55c 之后）：readers/search-backend/repo-utils 抽象未逐层对齐（S2 已补 SessionInfo/forkFrom/createBranchedSession/deferred 物化）
- [ ] coding-agent facade 纵向：open 自动 restore、缺凭证 typed 错误、fork/事件/shutdown（S6/S4）
- [ ] pi-ai breadth：三协议之外的 chat API 与 image API（明确排除项）

Rust-only 处置（S0 拍板）：

- [x] `SessionRepository::search()` 删除（S2，2026-08-01）
- [ ] `output_guard` 维持默认关闭（未接入任何工具输出路径）——冻结
- [ ] hashline / 进程内 grep/find / file mutation queue 保留并冻结，不继续扩展

工程化余项：

- [ ] 与 Pi TS 的差分测试（相同输入 → 相同输出）
- tool schema 对齐与 `schemars` 已从待办/验收项删除（S0 拍板：以手写 serde_json::json! 为准）

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
2. ✅ `cargo test -p pi` 所有测试通过（计数见文首「当前规模」，随实现滚动更新）
3. ✅ 用 mock `StreamFn` 运行完整 agent loop，验证双循环状态机行为
4. ✅ 用临时目录测试 JSONL session 持久化
5. ✅ 用 mock `ExecutionEnv` 测试工具执行管道
6. ✅ 测试 compaction 切点选择和摘要生成
7. ⬜ 差分测试：相同输入下 Pi TS 与 Pi Rust 输出一致性
8. ✅ 集成测试：`crates/pi/tests/`（hashline 工具真实文件系统 5 个 + anthropic live 3 个 ignored）；`examples/` 提供多轮 loop 手动验证入口
