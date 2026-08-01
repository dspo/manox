# TS Pi → crate pi 对齐清单

> 目的：逐项核验 `crates/pi` 对 TS Pi（pi-mono 的 `packages/ai` + `packages/coding-agent`）核心行为的对齐状态。
> 对齐基线：TS Pi upstream **`bf4a90d81985bd45052eeeae59d84fe13e0bd2c8`**（2026-08-01 第四轮对抗审查推进基线；此前基因为 `ea781d68f296bea36db3a540d2c53746f1a90bdd`）。
> 本文件只追踪 TS Pi → crate pi 的行为对齐。「pi crate 何时能替换 manox agent crate」的产品能力清单见 [manox-cutover.md](manox-cutover.md)。
> 状态图例：✅ 已对齐 · 🟡 已对齐但有记录在案的偏差 · 🔲 未对齐 · 🚫 有意偏离（记录理由）

## 1. Agent loop（`agent_loop.rs` ↔ agent-loop.ts）

| 能力 | 状态 | 说明 |
|---|---|---|
| 双循环结构 | ✅ | 外层 follow-up 循环 + 内层 tool call/steering 循环 |
| 队列排空时机 | ✅ | steering 在 tool call 边界注入，follow-up 在将停时注入；agent_end 前两队列排空 |
| 截断安全 | ✅ | stop_reason == length 时 fail 全部 tool calls |
| 中止检查 | ✅ | 批次级 CancellationToken 检查；取消中断执行中的工具（进程组树杀，对齐 TS killProcessTree） |
| 工具 progress | ✅ | 同步 emit 经 unbounded channel 实时转发，全部 update 先于 ToolExecutionEnd 结算（TS settled updateEvents 同序） |

## 2. Agent（`agent.rs` ↔ agent.ts）

| 能力 | 状态 | 说明 |
|---|---|---|
| 状态归约 | ✅ | transcript 仅经 MessageEnd 增长；streaming/pending/error 随事件更新（对齐 TS processEvents） |
| 流中事件 | ✅ | MessageUpdate 携 9 变体 AssistantMessageEvent（text/thinking/toolcall × start/delta/end），三 provider 形状各自映射 |
| 事件订阅 | ✅ | 监听器按注册序 await；agent_end 监听器结算后才算 idle |
| 队列与 RunHandle | ✅ | steering/follow-up 队列 Arc 共享，运行中可经 RunHandle 写入 |
| reset 语义分层 | ✅ | `clear_transcript_state`（清 transcript/流态，**保队列**）与 `reset`（全清）分离；压缩与 session restore 只走前者——queued 用户输入不会在压缩窗口丢失（2026-07-31） |

## 3. Harness 编排（`harness.rs` ↔ agent-session.ts）

| 能力 | 状态 | 说明 |
|---|---|---|
| Phase 状态机 | ✅ | idle/turn/compaction/branch_summary/retry；结构化操作 idle 门控 |
| overflow compact-retry | ✅ | 一次性预算 + 同模型/stale/aborted 守卫；摘除失败终端（session 保留）→ 压缩 → 重试一次；成功 assistant 重新武装 |
| threshold compact-no-retry | ✅（2026-07-31） | settled 回合（成功/错误）后计量超阈值即为下一回合压缩，不重试；维护性压缩失败只记日志，已完结回合结果不受影响（对齐 TS `_runAutoCompaction` catch → return false） |
| 压缩期队列处理 | ✅（2026-07-31） | 压缩只替换 transcript 不清队列；settle 循环最外层检查 queued 消息，非空即续跑一次 continuation 投递（对齐 TS `_handlePostAgentRun` 末行 `hasQueuedMessages()`，2026-08-01 从 threshold 分支提升到最外层） |
| session auto-retry | ✅（2026-08-01） | retryable 错误（overload/429/5xx/传输中断，排除 overflow 与 quota/billing）进入退避重试：错误留在 session 但离开重试上下文、默认 3 次 / 2s 指数退避、`abort()` 可取消退避、`RetryEvent` observer 承载 auto_retry_start/end 生命周期（对齐 TS `_prepareRetry` + `_retryAttempt`） |
| Hook 系统 | 🟡 | 结果承载 hook 全对齐（before_agent_start 注入/systemPrompt 覆盖、tool_call block、tool_result 全字段 patch、session_before_compact cancel/override、session_after_compact）；推迟项见 §8 |
| 持久化粒度 | 🟡 | turn 末批量追加；TS 在 message_end 逐条持久化，turn 中途崩溃不丢。对齐项见「已知余项」 |
| 运行配置 | ✅ | set_model/set_active_tools 持久化 entry 并校验；restore 回放 thinking tier/active tools/model（ModelResolver 插接，crate registry-free），不追加 entry |

## 4. Compaction（`compaction/` ↔ compaction.ts）

| 能力 | 状态 | 说明 |
|---|---|---|
| token 计量 | ✅ | calculate_context_tokens / estimate_context_tokens（usage 锚定 + 尾部字符启发 + stale 锚守卫）/ estimate_tokens / should_compact 逐项对齐 |
| 切点选择 | ✅ | find_cut_point 尾向头安全切点（不切 tool result 序列中） |
| 空范围拒绝 | ✅ | NothingToCompact typed 错误（对齐 TS prepareCompaction → undefined），summary 载体不重复折叠 |
| 摘要请求 | ✅ | serialize_conversation 全消息形状 + SUMMARIZATION_SYSTEM_PROMPT + 双模板（previous-summary 切换 UPDATE 变体）逐字对齐 |
| 边界持久化 | ✅ | CompactionEntry v3 schema（含 retainedTail?）；restore 走 firstKeptEntryId 重建或直接投影 retainedTail |
| split-turn | 🔲（P2） | cut 恒在整轮边界；单个超大工具轮超 keep-recent 时压缩无法真正缩小上下文。见「已知余项」 |

## 5. Session tree（`session/` ↔ session-manager）

| 能力 | 状态 | 说明 |
|---|---|---|
| v3 schema | ✅ | 全部 entry 变体逐字段对齐（camelCase rename 全覆盖，parentId 不丢 ancestry） |
| JSONL 存储 | ✅ | 追加写入 + leaf 游标；get_path 全路径 walk，未知 leaf/断链显式报错 |
| 上下文重建 | ✅ | get_branch / build_context_entries / build_session_context 对齐 TS；设置类 entry（thinking_level/model_change/active_tools_change）经 SessionContext 上报不回 transcript |

## 6. Providers（`provider/` ↔ packages/ai）

| 能力 | 状态 | 说明 |
|---|---|---|
| Anthropic 形状 | ✅ | content_block_start 自带 text/thinking/signature 保留，signature_delta 追加（对齐 upstream 59ad3dead，2026-07-31）；refusal 的 `stop_details.explanation` 进入 error_message、`rawStopReason` 持久化、redacted thinking 用 `[Reasoning redacted]` 占位（2026-08-01） |
| Completions 形状 | 🟡 | 已对齐；已知偏差：交错块模型（TS 每条流只合并一个 text/thinking 块，crate pi 交错时另开新块），见「已知余项」；截断流（无 `[DONE]` 且无 `finish_reason`）报 transport 错误而非当成功响应（2026-08-01） |
| Responses 形状 | ✅ | reasoning encrypted_content 往返、tool call id 规则、孤儿 call 合成结果等逐项对齐 |
| 握手重试 | ✅ | 形状无关装饰器：429/408/5xx + 连接期传输错误指数退避、Retry-After 遵从、6 次上限、仅握手阶段 |
| 溢出分类 | ✅ | 20 种跨厂商子串 + 限流排除 + 413；terminal/mid_stream 两构造点统一 ProviderError::Overflow |
| Prompt caching | ✅ | 按 TS Pi 对齐：CacheRetention 三态挂 AgentContext；Anthropic 三断点、Completions prompt_cache_key 门控、Responses 无 URL 门控 |
| 悬空 tool call 修复 | ✅ | 请求线边界共享 repair_tool_flow（对齐 TS transformMessages 第二趟）+ error/aborted 剥离 |

## 7. 工具与设置

| 能力 | 状态 | 说明 |
|---|---|---|
| 7 内建工具 | ✅ | read/write/edit/bash/grep/find/ls + truncate/path_utils/file_mutation_queue 基础设施 |
| Settings 合并 | ✅ | 只合并显式字段（field-wise CompactionOverrides，对齐 TS deepMergeSettings 递归语义） |
| cache_stats | 🟡 | token 维度 miss 检测已对齐；金额/idle 未接线，见「已知余项」 |

## 8. 有意偏离（🚫，不视为对齐缺口）

- **Edit 工具**：hashline 锚定补丁 + 3-way 快照恢复，替代 TS 的 string-replace（manox 增强，2026-07-29 拍板）
- **grep/find 进程内化**（ignore + regex + globset），TS shell 出系统 grep/find
- **manox 自创恢复项不移植**：空响应 nudge、拒绝熔断、取消级联清理（TS agent-loop 皆无）
- **registry 不进 crate**：模型解析经 consumer 插接的 ModelResolver，crate 保持 registry-free
- **Hook 推迟项**：`before_provider_payload`/`after_provider_response`（provider 层无接缝）、`session_before_tree`/`session_tree`（无 tree 操作）、`summarization_retry_*`（summarization 调用无重试/取消）、`model_update`/`tools_update`（fire 点已具备、变体未接线）、`thinking_level_update`/`resources_update`（无 setter 面）——激进纪律不留无 fire 点的死变体；session auto-retry 已由 `RetryEvent` observer 承载（2026-08-01）

## 已知余项（对齐缺口，按严重度排序）

1. **split-turn（P2）**：turn prefix 摘要。cut 恒在整轮边界，单轮超 keep-recent 窗口时整轮保留——极端工具轮压缩后仍可能溢出，决定「压缩能否真正缩小上下文」
2. **session 逐条 append**：TS 在 message_end 即持久化；crate pi turn 末批量追加，turn 中途崩溃丢失该 turn 消息
3. **pre-prompt 压缩检查**：TS `prompt()` 发送前 `_checkCompaction(lastAssistant, skipAbortedCheck=false)` 兜住 aborted 回合（aborted 回合跳过了回合后检查）；crate pi 只有回合后检查，aborted 后超阈值需等真实溢出（2026-07-31 记录在案）
4. **cache_stats 金额与 idle**：missed_cost 恒 0、idle_ms 占位，需接线 ModelPriceSource 与消息时间戳
5. **Completions 流交错块模型**：TS 每条流只合并一个 text/thinking 块（交错并入同块），crate pi 交错时关闭当前块另开新块，终态 content 形状不同
6. **Hook 推迟项**：见 §8——model_update/tools_update 的 fire 点已具备，可补变体接线

## 工程化余项（非行为对齐）

- 与 TS Pi 的差分测试（相同输入 → 相同输出）
- `schemars` 从 Rust struct 派生工具 JSON Schema（替代手写 serde_json::json!）

## 审查历史

| 轮次 | Rust 基线 | TS 基线 | 结果 |
|---|---|---|---|
| 第四轮（2026-08-01） | `fe4431d` | `bf4a90d8`（新对齐基线） | 3 P1（post-run 队列投递 / Completions 截断流 / session auto-retry）+ 1 P2（Anthropic refusal 细节）均已修复，见各章节 |
| 第三轮（2026-07-31） | `36884be` | `ea781d68`（新对齐基线） | 3 P1（压缩清队列 / content_block_start 丢初始内容 / threshold 未接线）+ 1 P2（本文档拆分），均已修复 |
| 第二轮（2026-07-31） | round-1 修复后 | 7df73a00c（本地 checkout） | 7 项修复合并为 `36884be`，remora 零缺陷 |
