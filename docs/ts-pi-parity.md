# TS Pi → crate pi 对齐清单

> 目的：逐项核验 `crates/pi` 对 TS Pi（pi-mono 的 `packages/ai` + `packages/coding-agent`）核心行为的对齐状态。
> 对齐基线：TS Pi upstream **`4488ad55c18f07ae89a489096c90de8667b3adfb`**（2026-08-01 第五轮对抗审查推进基线；此前基因为 `bf4a90d8`，再前为 `ea781d68`）。
> 本文件只追踪 TS Pi → crate pi 的行为对齐。「pi crate 何时能替换 manox agent crate」的产品能力清单见 [manox-cutover.md](manox-cutover.md)。
> 状态图例：✅ 已对齐 · 🟡 已对齐但有记录在案的偏差 · 🔲 未对齐 · 🚫 有意偏离（记录理由）
> 基线冻结（S0，2026-08-01）：Rust HEAD `b9b6869` + 未提交 S0 校准（10 改 2 增）；TS `4488ad55c` 不变。冻结后其余 agent 以此共享基线开始。

## 1. Agent loop（`agent_loop.rs` ↔ agent-loop.ts）

| 能力 | 状态 | 说明 |
|---|---|---|
| 双循环结构 | ✅ | 外层 follow-up 循环 + 内层 tool call/steering 循环 |
| 队列排空时机 | ✅ | steering 在 tool call 边界注入，follow-up 在将停时注入；agent_end 前两队列排空 |
| 截断安全 | ✅ | stop_reason == length 时 fail 全部 tool calls |
| 失败终态保留 partial | ✅（2026-08-01） | provider 中途失败时基于已流出的最新 partial Assistant 构造终端 error（保留 content/usage/response id/api/timestamp，只覆盖 stop reason 与 error message），对齐 TS catch 原位改写路径 |
| 中止检查 | ✅ | 批次级 CancellationToken 检查；取消中断执行中的工具（进程组树杀，对齐 TS killProcessTree） |
| 工具 progress | ✅ | 同步 emit 经 unbounded channel 实时转发，全部 update 先于 ToolExecutionEnd 结算（TS settled updateEvents 同序） |

## 2. Agent（`agent.rs` ↔ agent.ts）

| 能力 | 状态 | 说明 |
|---|---|---|
| 状态归约 | ✅ | transcript 仅经 MessageEnd 增长；streaming/pending/error 随事件更新（对齐 TS processEvents） |
| 流中事件 | ✅ | MessageUpdate 携 9 变体 AssistantMessageEvent（text/thinking/toolcall × start/delta/end），三 provider 形状各自映射 |
| 事件订阅 | ✅ | 监听器按注册序 await；agent_end 监听器结算后才算 idle |
| 队列与 RunHandle | ✅ | steering/follow-up 队列 Arc 共享，运行中可经 RunHandle 写入 |
| prepare-next-turn | ✅（2026-08-01） | `prepare_next_turn` 接入 `LoopHooks`/`create_loop_config`；`ChannelSink` 每事件 ack 在归约+listener 结算后返回，循环下一步必然观察到 listener 副作用（对齐 TS awaited emit）；Harness 经 `HarnessHandle::set_model/set_thinking_level` 更新共享 TurnRuntime 快照，下一轮 provider 请求前刷新 context，持久化队列逐条成功才 pop |
| provider runtime 切换 | ✅（2026-08-01） | `Model.api` discriminator + consumer 插拔 `StreamResolver`：正常 turn、overflow retry、continuation、summarization 每次请求按当前 model 解析 StreamFn（协议/endpoint/credential 随之切换）；无 resolver 时回落构造期 StreamFn；resolver 失败生成 terminal `Assistant(Error)` 并正常发出 MessageEnd/TurnEnd/AgentEnd；无 resolver 时跨 API 切换显式报错（idle set_model 返回 Err、handle 拒绝）；Model 全字段判等（provider/api/id/运行参数）
| reset 语义分层 | ✅ | `clear_transcript_state`（清 transcript/流态，**保队列**）与 `reset`（全清）分离；压缩与 session restore 只走前者——queued 用户输入不会在压缩窗口丢失（2026-07-31） |

## 3. Harness 编排（`harness.rs` ↔ agent-session.ts）

| 能力 | 状态 | 说明 |
|---|---|---|
| Phase 状态机 | ✅ | idle/turn/compaction/retry/branch_summary 均有真实路径（navigate_tree 设 branch_summary）；结构化操作 idle 门控 |
| overflow compact-retry | ✅ | 一次性预算 + 同模型/stale/aborted 守卫；摘除失败终端（session 保留）→ 压缩 → 重试一次；成功 assistant 重新武装 |
| threshold compact-no-retry | ✅（2026-07-31） | settled 回合（成功/错误）后计量超阈值即为下一回合压缩，不重试；维护性压缩失败只记日志，已完结回合结果不受影响（对齐 TS `_runAutoCompaction` catch → return false） |
| 压缩期队列处理 | ✅（2026-07-31） | 压缩只替换 transcript 不清队列；settle 循环最外层检查 queued 消息，非空即续跑一次 continuation 投递（对齐 TS `_handlePostAgentRun` 末行 `hasQueuedMessages()`，2026-08-01 从 threshold 分支提升到最外层） |
| session auto-retry | ✅（2026-08-01） | retryable 错误（overload/429/5xx/传输中断，排除 overflow 与 quota/billing）进入退避重试：错误留在 session 但离开重试上下文、默认 3 次 / 2s 指数退避、`RetryEvent` observer 承载 auto_retry_start/end 生命周期（对齐 TS `_prepareRetry` + `_retryAttempt`）；`HarnessHandle::abort`/`wait_for_idle` 统一覆盖 agent run、退避与 settle（对齐 TS `abort()` → `abortRetry()` + `waitForIdle()`）；cancel token 先于 Start 事件安装，listener 的 abort 必取消当前退避（2026-08-01） |
| Hook 系统 | 🟡 | 结果承载 hook 全对齐（before_agent_start 注入/systemPrompt 覆盖、tool_call block、tool_result 全字段 patch、session_before_compact cancel/override、session_after_compact）；推迟项见 §9 |
| 持久化粒度 | ✅（2026-08-01） | message_end 逐条 append（harness persistence middleware 先于 listener）；turn 中途崩溃只丢未完成消息，已提交消息全部可恢复；mutation 在下一 provider request 前 flush（prepare-next-turn），model_change 先于新模型消息 |
| 运行配置 | ✅ | set_model/set_active_tools/set_thinking_level idle 期持久化并应用；运行中 model/thinking/active_tools 经 HarnessHandle 立即更新共享快照、下一轮生效（prepare-next-turn 在下一 provider 请求前刷新 context；active_tools 运行中排队 2026-08-01 闭环），独立 continuation 首轮亦生效（apply_turn_runtime 在新 run 前同步）；持久化队列逐条成功才 pop，flush 失败后 restore 重放队列到共享快照，恢复后的下一 provider 请求仍由被排队 model 服务；`restore()` 同步 Agent/Harness/共享快照三者 |

## 4. Compaction（`compaction/` ↔ compaction.ts）

| 能力 | 状态 | 说明 |
|---|---|---|
| token 计量 | ✅ | calculate_context_tokens / estimate_context_tokens（usage 锚定 + 尾部字符启发 + stale 锚守卫）/ estimate_tokens / should_compact 逐项对齐 |
| 切点选择 | ✅ | find_cut_point 尾向头安全切点（不切 tool result 序列中） |
| 空范围拒绝 | ✅ | NothingToCompact typed 错误（对齐 TS prepareCompaction → undefined），summary 载体不重复折叠 |
| 摘要请求 | ✅ | serialize_conversation 全消息形状 + SUMMARIZATION_SYSTEM_PROMPT + 双模板（previous-summary 切换 UPDATE 变体）逐字对齐 |
| 边界持久化 | ✅ | CompactionEntry v3 schema（含 retainedTail?）；restore 走 firstKeptEntryId 重建或直接投影 retainedTail |
| split-turn | ✅（2026-08-01） | 超大工具轮 cut 拆出 turn prefix 单独摘要（history + prefix 双调用合并）；tool chain 保留在 retained tail；`split_turn_compact` example（90k→179 tokens） |

## 5. Session tree（`session/` ↔ session-manager）

| 能力 | 状态 | 说明 |
|---|---|---|
| v3 schema | ✅ | 全部 entry 变体逐字段对齐（camelCase rename 全覆盖，parentId 不丢 ancestry） |
| JSONL 存储 | ✅ | 追加写入 + leaf 游标；get_path 全路径 walk，未知 leaf/断链显式报错；append 事务线性化：Session 层串行化 parent-selection + append（并发 append 成链不 fork 兄弟分支），存储层 write→index→cursor 原子（对齐 TS 4488ad55c 的 linear-time 修复，2026-08-01）；load/append 拒绝重复或空 entry id；wire 级校验：header id/cwd 非空、metadata 为对象、entry `parentId`（leaf 另含 `targetId`）必须为 null|string，缺失即 corruption，`metadata:null`/`parentSession:null` 拒绝而缺失接受（对齐 TS parseHeaderLine/parseEntryLine，2026-08-01）；create 与 open 共用同一 header validator，不再写出自身拒绝的文件 |
| 上下文重建 | ✅ | get_branch / build_context_entries / build_session_context 对齐 TS；设置类 entry（thinking_level/model_change/active_tools_change）经 SessionContext 上报不回 transcript |
| SessionRepository create/open/list/delete | ✅（S2，2026-08-01） | `create`/`open`/`list`/`delete`（`session/repository.rs`）；`list` 对齐 TS `SessionInfo` 全字段（path/id/cwd/name/parentSessionPath/created/modified/messageCount/firstMessage/allMessagesText）按 modified 倒序、坏文件跳过；新 session 走 deferred-first-assistant（TS `_persist`：文件在第一条 assistant 时物化，空 session 不出现在 list） |
| SessionRepository fork | ✅（S2，2026-08-01） | `fork_from(source, target_cwd)` 对齐 TS `forkFrom`：新 id + 新 timestamp + 目标 cwd + `parentSession` 为源文件路径 + 全量非 header entries 复制（eager）；`create_branched_session(leaf_id)` 对齐 TS `createBranchedSession`：root→leaf 路径、label 剥离重链 parentId、为保留节点重建 label entries（保留原 timestamp）、deferred 物化；`ForkPosition::BeforeUser`/`AtEntry` 的 AgentSession 入口见 S6 |
| SessionRepository search | 🚫 已删除（S2） | Rust-only 扩展，TS 核心无此 API；公开入口已移除 |
| Session move_to/label/name/stats/pagination/custom | ✅ | `Session::move_to/append_label/set_session_name/stats/page/custom`（`session/mod.rs`）；对应单测与 examples 覆盖 |
| 分支查询 | ✅（S8，2026-08-01） | `Session::find_entries_on_branch` / `find_entry_on_branch`（`SessionBranchQuery`：start/stopAtType/stopAtId/type/customType/order/limit），对齐 upstream bounded branch queries |
| branch_summary append | ✅ | 摘要输入/提示词/结果按 TS 对齐（S1，2026-08-01）：`getMessageFromEntry`（tool result 排除、custom/branch/compaction carrier）、`prepareBranchEntries`（token budget + carrier 90% 规则 + read/modified file ops 累积）、`BRANCH_SUMMARY_PROMPT`/preamble、`serialize_conversation` 会话、usage/readFiles/modifiedFiles 随 entry 持久化；差分 fixture `branch-summary-preparation.txt` |
| navigate_tree | 🟡 | 默认 summarize=false、选项面、`NavigateTreeResult`（cancelled/aborted/editor_text/summary_entry_id）、BranchSummary phase、label 落盘（summary/target entry）、abort 取消（不动游标不追加 entry）、`session_before_tree`（cancel/override）/`session_tree` typed hook 已对齐（S1，2026-08-01）；余项：summarization 无 retry 策略（见「已知余项」） |

## 6. Providers（`provider/` ↔ packages/ai）

| 能力 | 状态 | 说明 |
|---|---|---|
| Anthropic 形状 | ✅ | content_block_start 自带 text/thinking/signature 保留，signature_delta 追加（对齐 upstream 59ad3dead，2026-07-31）；refusal 的 `stop_details.explanation` 进入 error_message、`rawStopReason` 持久化、redacted thinking 用 `[Reasoning redacted]` 占位（2026-08-01）；terminal guard：缺 `message_stop` 或缺 stop reason 的流报 retryable mid-stream 错误，部分回复不再持久化为成功；尾部未闭合 SSE 解析失败照常传播（对齐 TS 2026-08-01） |
| Completions 形状 | ✅ | 单 text + 单 thinking 块合并交错（对齐 TS，2026-08-01）；缺 `finish_reason` 的流（含 `[DONE]`-only）默认报 transport 截断，仅显式 `with_supports_finish_reason(false)` 才推断 stop/toolUse（对齐 TS `supportsFinishReason` 默认 true） |
| Responses 形状 | ✅ | reasoning encrypted_content 往返、tool call id 规则、孤儿 call 合成结果等逐项对齐 |
| 握手重试 | ✅ | 形状无关装饰器：429/408/5xx + 连接期传输错误指数退避、Retry-After 遵从、6 次上限、仅握手阶段 |
| 溢出分类 | ✅ | 20 种跨厂商子串 + 限流排除 + 413；terminal/mid_stream 两构造点统一 ProviderError::Overflow |
| Prompt caching | ✅ | 按 TS Pi 对齐：CacheRetention 三态挂 AgentContext；Anthropic 三断点、Completions prompt_cache_key 门控、Responses 无 URL 门控 |
| 悬空 tool call 修复 | ✅ | 请求线边界共享 repair_tool_flow（对齐 TS transformMessages 第二趟）+ error/aborted 剥离 |
| 请求级 options | ✅（S3，2026-08-01） | `StreamOptions`（headers/timeout/max_tokens）从 harness turn snapshot 流入每次请求（`AgentContext.stream_options` overlay 于 builder options；idle setter + `HarnessHandle::set_stream_options` 运行中生效） |
| 请求 hooks | ✅（S3，2026-08-01） | `RequestObserver`（before_payload / after_response）在每次 attempt 触发（含重试）；provider builder 挂接，harness `request_observer()` 映射到 `BeforeProviderPayload`/`AfterProviderResponse` hook 点 |

## 7. 工具与设置

| 能力 | 状态 | 说明 |
|---|---|---|
| 7 内建工具 | ✅ | read/write/edit/bash/grep/find/ls + truncate/path_utils/file_mutation_queue 基础设施 |
| Settings 合并 | 🟡 | `settings.rs` 只做结构合并（field-wise CompactionOverrides，对齐 TS deepMergeSettings 递归语义）；文件加载/保存/reload/递归覆盖由接线方负责——见 S5 |
| Trust | 🟡 | `trust.rs` 内存态决策管理，无持久化、未接入资源/工具启用策略——见 S5 |
| cache_stats | ✅（S5） | token 维度 miss 检测 + `StaticModelPrices` 真实计价（missed_cost）+ idle；`CacheWasteTotals` 消费侧由 UI 负责 |
| output_guard | 🚫 默认关闭 | `output_guard.rs` 未接入任何工具输出路径（无可观察行为）；维持默认关闭，除非差分测试证明不改变 TS 模型可见行为 |
| hashline edit | 🚫 接受并冻结 | hashline 锚定补丁 + 3-way 快照恢复（manox 增强，2026-07-29 拍板），保留但冻结不扩展 |

## 8. coding-agent facade（`coding_agent/` ↔ packages/coding-agent/src/core）

| 能力 | 状态 | 说明 |
|---|---|---|
| AgentSession build | 🟡 | `AgentSession::build`（`coding_agent/agent_session.rs`）默认 model 回退 env `ANTHROPIC_MODEL`、session 目录 `.pi-sessions`；settings/trust/resources/system prompt/tools 完整组装见 S6 |
| AgentSession open | ✅ | `open()` 返回前自动 `restore()` 并复用完整装配（settings/trust/resources/prompt/tools/runtime）；model 经 catalog 精确恢复（未知显式报错，不静默回退） |
| ModelRuntime from_env | ✅（S3，2026-08-01） | `from_env()` 缺凭证返回 typed `MissingCredential`（命名缺失 env var），不再生成 `missing-*_API_KEY` 假值；自定义 registry 不受 env 限制 |
| ResourceLoader | 🟡 | `ResourceLoader`（`coding_agent/resources.rs`）有类型与 CLAUDE.md 加载；global context/ancestor chain/skills/templates/去重/诊断未真实接线——见 S5 |
| settings/trust/cache | ✅（S5/S8/S11） | settings 文件 load/save/reload + camelCase；trust 全局 agentDir/trust.json + 祖先匹配 + untrusted 门控项目 settings/资源；cache 真实计价（StaticModelPrices）+ idle |
| fork / events / shutdown | ✅（S4/S6） | `fork(entry_id, ForkPosition)`（BeforeUser 返回选中文本）、HarnessEvent 订阅、shutdown（幂等/清队列/清 mutation/拒绝新操作） |

## 9. 有意偏离（🚫，不视为对齐缺口）

- **Edit 工具**：hashline 锚定补丁 + 3-way 快照恢复，替代 TS 的 string-replace（manox 增强，2026-07-29 拍板）
- **grep/find 进程内化**（ignore + regex + globset），TS shell 出系统 grep/find
- **manox 自创恢复项不移植**：空响应 nudge、拒绝熔断、取消级联清理（TS agent-loop 皆无）
- **registry 不进 crate**：模型解析经 consumer 插接的 ModelResolver，crate 保持 registry-free
- **Hook 推迟项（已收窄）**：`before_provider_payload`/`after_provider_response` 已接 RequestObserver（S3/S8/S10，带 model + headers map）；`session_before_tree`/`session_tree` 已接 typed hooks（S1/S10，含 summary override + fromHook）；`summarization_retry_*` 已由 RetryEvent observer 承载（S10）；`model_update`/`tools_update`/`resources_update` 已由 HarnessEvent 承载（S4）；`thinking_level_update` 未单独发事件（随 thinking setter 的 HarnessEvent 覆盖）。剩余：extension loader 相关变体（动态 extension 明确不移植）

## 已知余项（对齐缺口，按严重度排序）

1. ~~cache_stats 金额与 idle~~：`StaticModelPrices` + missed_cost + idle 已接线（S5）；session stats 消费侧未做（UI 职责）
2. **summarization retry/cancel**：summarization 与 branch summary 调用无 retry 策略与取消通道（S1）
3. **summarization retry**：branch summary 与 compaction 的 summarization 调用无 retry 策略（S1 已接 abort/取消，retry 仍缺）
4. **Session store/reader/repository 深度（upstream 4488ad55c 之后）**：crate pi 已有 SessionRepository + Session 树操作；TS 的 readers/search-backend/repo-utils 抽象未逐层对齐；Rust-only `search()` 标删除（S2）
5. ~~coding-agent facade 纵向~~：open 自动 restore、typed credential、fork/事件/shutdown 已闭环（S6/S10）；model catalog 需 consumer 注入精确表（S12）
6. **pi-ai breadth**：已选三协议之外的 7 个 chat API + image API 未实现（明确排除项，不阻塞 agreed scope）
7. **upstream delta `4488ad55c..origin/main`（已推进至 `c6eb6281`，审计于 S7/S10）**：① session storage repository 重构（jsonl-store→jsonl-repo、memory-repo、search.ts，applicable unported delta）；② bounded branch queries + SQLite branch caching（Rust 已移植查询语义，SQLite 缓存未做）；③ harness v2 文档（设计，未实现）；④ coding-agent 修复（connection timeout、availability refresh、model-runtime）；⑤ post-login model catalog refresh 15s 超时与取消信号（env-only scope 下 deferred）。基线冻结在 `4488ad55c`，以上拆为独立 ledger 项，不夹带已验收切片

## 工程化余项（非行为对齐）

- 与 TS Pi 的差分测试（相同输入 → 相同输出）
- tool schema 对齐与 `schemars` 已从待办/验收项删除（S0 拍板：以手写 serde_json::json! 为准）

## 审查历史

| 轮次 | Rust 基线 | TS 基线 | 结果 |
|---|---|---|---|
| S0（收口校准，2026-08-01） | `b9b6869` + dirty（10 改 2 增，未提交） | `4488ad55c`（未变） | 修 fixture 尾随空格（`git diff --check` 通过）；repository/navigation、coding-agent facade、settings/trust/cache 逐条状态化（🟡/🚫 不再用模块级 ✅ 掩盖）；`search` 标删除、`output_guard` 标默认关闭、hashline 标接受并冻结；schemars 从待办删除；**S0 完成后冻结共享基线，其余 agent 在此基线上开始** |
| S1（2026-08-01） | `4033883` | `4488ad55c` | branch summarization 按 TS 重写（getMessageFromEntry/prepareBranchEntries/BRANCH_SUMMARY_PROMPT/preamble/usage/read+modified files）；navigate label/abort/hook（session_before_tree/session_tree）；append_branch_summary details 对齐 TS wire shape；差分 fixture branch-summary-preparation |
| S2（2026-08-01） | `beb2f3d` | `4488ad55c` | SessionInfo 全字段 + modified 倒序 + 坏文件跳过；fork_from（parentSession=源路径/新 timestamp/目标 cwd）；create_branched_session（label 重链 + deferred 物化）；deferred-first-assistant 持久化；search 删除 |
| S3（2026-08-01） | `b0d593a` | `4488ad55c` | per-request StreamOptions（turn snapshot → 每次请求，overlay builder）；ModelRuntime::from_env typed MissingCredential；RequestObserver（before_payload/after_response 每次 attempt）+ harness hook 映射 |
| S4（2026-08-01） | `4671f7e` | `4488ad55c` | Agent 队列模式 getter/setter/clear；HarnessEvent 订阅（QueueUpdate/Settled/Model/Thinking/Tools/ResourcesUpdate）；shutdown（幂等、清队列、取消、拒绝新操作）；skill_with_instructions、append_message（idle 立即/运行中 mutation 队列）、set_tools 校验 |
| S5（2026-08-01） | `f569503` | `4488ad55c` | ResourceLoader（agentDir + AGENTS/CLAUDE ancestor chain、候选顺序、canonical 去重、递归 SKILL.md + frontmatter + 冲突诊断）；settings 文件 load/save/reload 递归合并；trust JSON 持久化 + untrusted；cache_stats 真实计价（StaticModelPrices）与 idle |
| S6（2026-08-01） | `bd936bc` | `4488ad55c` | AgentSession 纵向：build 全组装（settings/trust/resources/model/runtime，override 优先）；open 自动 restore；S4 面转发；fork(entry, ForkPosition)（BeforeUser 返回选中文本）；async close settle；缺凭证早失败；untrusted 禁用副作用工具 |
| S7（2026-08-01） | `76e787c` | `4488ad55c` | check_examples.sh 离线 example gate（8 个实际运行）；fixture README 记录生成命令；upstream delta audit 拆为独立 ledger 项 |
| S8（复核轮，2026-08-01） | `7f9e077` | `4488ad55c` | 按 remora 复核修复公共组装路径：session cwd 注入工具环境；AGENTS/CLAUDE 指令进 system prompt（不再作 skill）、`.pi/skills|prompts` 目录对齐；settings camelCase + `~` 展开 + thinking/compaction/retry/queue modes 应用；trust 对齐 TS（agentDir/trust.json + 祖先匹配 + undecided 门控项目资源，移除删工具行为）；惰性 per-model 凭证 + provider+modelId 恢复完整 API；fork 保留 assembly 状态；observer 接入公共路径（payload 可变 + headers）；navigate 取消/重试/请求选项闭环；shutdown 清 pending mutation；fork label 最终态；bounded branch queries（find_entries_on_branch/find_entry_on_branch） |
| S9（复核轮，2026-08-01） | `d86a6fe` | `4488ad55c` | build/open 共用 assemble（open 按 session model 经 catalog 解析，不再默认 Anthropic 阻塞；AgentSession.cwd 跟 session）；branch query 重写为 upstream 语义 + 移植 branch-query.test.ts；唯一 system-prompt builder 雏形；settings.skills/prompts 初接线；before-payload 链式传递 + after-response headers；branch-summary retry 收窄 + 溢出安全退避 |
| S12（复核轮，2026-08-01） | `9d54236` | `4488ad55c` | 分层 next-turn（harness queued-first 恢复 + facade user-first asides，`PromptInput.asides`）；七工具 registry + 初始四工具 active subset（`set_initial_active_tools` 内存）；thinking presence witness（`has_thinking_entry`，显式 off 不被 settings 覆盖）+ 默认 medium clamp + 新 session 初始 model/thinking entry 持久化；默认 system prompt 内容（身份/guidelines，`DEFAULT_BASE_PROMPT`）；默认 catalog 精确表（不猜任意 ID）+ reopen miss 回退初始模型 + `modelFallbackNotice`；fork 继承 prompt builder + `AssemblyConfig::apply` 传播错误 |
| S11（复核轮，2026-08-01） | `0443948` | `4488ad55c` | trust 先于 settings（untrusted 项目 settings 视为空配置，含回归）；default thinking 双状态（agent + turn_runtime）且 reopen 无 thinking entry 时回落 settings default；默认工具集改 TS 四件（read/bash/edit/write）；system prompt 随 active tools/resources 重建（builder 闭包，关闭 read 后隐藏 skills）+ skill XML 转义；reopen 模型 catalog 未命中显式报错（不再静默回退）；next_turn 顺序改 user 在前（TS coding-agent）；`prompt()` 默认展开 `/skill:` 与 `/template`；tree hook summary 仅在 summarize 时接受 + fromHook 贯穿持久化与事件；skill/template collision 保留先加载（TS winner=first）；env 测试共享锁消除并行污染 |
| S10（复核轮，2026-08-01） | `f154f9c` | `4488ad55c` | open 不再经持久化 setter 应用 settings default thinking（新 session 内存初始态，reopen 投影持久化 tier）；`ModelRuntime` 引入可注入 `ModelCatalog`（默认 catalog + 自定义 runtime 注入，同 ID 双协议 reopen 测试）；branch-summary retry：backoff 取消返回 cancelled/aborted 结果（非错误）、成功/失败 End 生命周期配对、maxTokens 强制 2048、quota/billing body 排除重试；settings extra path 支持 file-or-directory（不再多拼 skills/prompts 子目录）、ancestor context 顺序 root→cwd；system prompt 补 working dir/active tools/context 真实路径/skill XML + 无 read 工具隐藏 skills，删除未调用第二套 builder；tree hook 支持 summary override + replaceInstructions override；before-provider-payload 带 model、after-response headers 为 Record<string,string>；P3 测试改进（reopen 不再改进程 cwd、build_fails_early 真调 build） |
| 第十轮（2026-08-01） | `b9b6869` | `4488ad55c`（未变） | navigate_tree 默认 summarize=false + 选项面（custom/replace/label）+ BranchSummary phase；next_turn 运行中排队（HarnessHandle）；flush 失败 restore 重放队列（恢复后由被排队 model 服务）；substitute_args 单趟全语法（defaults/$0/不递归）；fixture 刷新链路可用（bun 捕获真实 TS 源码）；PLAN/parity 校准 |
| 第九轮（Phase 4A，2026-08-01） | `d100401` | `4488ad55c`（未变） | 运行配置闭环：Model 全字段判等、restore 三态同步、resolver 失败 terminal 化、无 resolver 跨 API 报错、harness_chat api 修正；新增 6 个回归 + `runtime_switch` example |
| Phase 4B（2026-08-01） | `faa9690` | `4488ad55c` | EventSink Result 化、PrepareTurnFn async+TurnUpdate、Agent middleware、message_end 逐条持久化（Arc Session）、删除 turn 末批量写；`session_resume` example |
| Phase 3A（2026-08-01） | `4b7605d` | `4488ad55c` | split-turn 双摘要（history+turn prefix）、pre-prompt aborted compaction；`split_turn_compact` example |
| Phase 3B（2026-08-01） | `8cf26ef` | `4488ad55c` | SessionRepository（create/open/list/delete/fork/search）、Session move_to/label/name/stats/pagination/custom、navigate_tree；`navigate_tree` example |
| Phase 4C（2026-08-01） | `e600989` | `4488ad55c` | PromptInput/images、skills/templates、next_turn、mid-run active-tools、session entry 入口 |
| Phase 2B（2026-08-01） | `bf98d4c` | `4488ad55c` | Completions 交错块合并（单 text+单 thinking）、请求级 headers/timeout |
| Phase 8（2026-08-01） | `562e3a0` | `4488ad55c` | coding-agent facade（AgentSession/Builder、ModelRuntime、ResourceLoader、create_agent_session）；`coding_agent_smoke` example |
| Phase 7（2026-08-01） | 终局 | `4488ad55c` | fixtures/ts-pi 差分（serialize_conversation/system prompt/file ops）+ 刷新脚本；最终门禁全绿 |
| 第八轮（2026-08-01） | `b2b3a10` | `4488ad55c`（未变） | 1 P1（StreamResolver：Model.api + 每请求 provider runtime 切换）+ 3 P2（flush 逐条成功才 pop / TurnRuntime 快照消除 continuation 晚一拍 / create-open header validator 统一+null 拒绝）+ 1 P3（PLAN 校准）均已修复，见各章节 |
| 第七轮（2026-08-01） | `af7c195` | `4488ad55c`（未变） | 2 P2（prepare-next-turn 接线+运行中 mutation 排队+thinking setter 闭环 / JSONL wire 结构校验）+ 1 P3（partial 错误保留 timestamp/api）均已修复，无新 P0/P1 |
| 第六轮（2026-08-01） | `9367432` | `4488ad55c`（未变） | 3 P2（partial 失败终态保留 / retry token 先于 Start 安装 / JSONL 重复 id 拒绝）+ 2 P3（尾部 SSE 解析错误传播 / 并发测试断言）均已修复，无新 P0/P1 |
| 第五轮（2026-08-01） | `54873cd` | `4488ad55c`（新对齐基线） | 4 P1（HarnessHandle 取消/等待 / Anthropic terminal guard / Session append 线性化 / Completions 严格 finish_reason）+ 2 P2（retry classifier 正则保真 / ledger 校准）均已修复，见各章节 |
| 第四轮（2026-08-01） | `fe4431d` | `bf4a90d8`（新对齐基线） | 3 P1（post-run 队列投递 / Completions 截断流 / session auto-retry）+ 1 P2（Anthropic refusal 细节）均已修复，见各章节 |
| 第三轮（2026-07-31） | `36884be` | `ea781d68`（新对齐基线） | 3 P1（压缩清队列 / content_block_start 丢初始内容 / threshold 未接线）+ 1 P2（本文档拆分），均已修复 |
| 第二轮（2026-07-31） | round-1 修复后 | 7df73a00c（本地 checkout） | 7 项修复合并为 `36884be`，remora 零缺陷 |
