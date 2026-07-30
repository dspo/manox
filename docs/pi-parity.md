# pi ↔ manox agent 能力对等清单

> 目的：定义「pi crate 何时成熟到可以替换 manox `agent` crate」的可核对条件。
> 状态图例：✅ 已对等 · 🟡 pi 已有但浅 · 🔲 缺失需移植 · 🚫 有意不做（不面向历史和负债） · ❓ 待决策
> 规模列为 manox 侧行数，用于估算移植成本。pi 当前共 8.1k 行，agent 共 54.5k 行。
> 更新方式：随 pi 落地逐项翻转状态；全部 ✅/🚫 之日即切换入场之时。

## 关键结构发现（影响移植策略）

1. **manox 的核心是 `thread.rs`——10,857 行的 gpui Entity 单文件**。循环、审批、压缩、事件全耦合在 gpui 里。pi 的 runtime-agnostic 设计（mpsc 事件 + 无 gpui 依赖）正是对此的修正，移植时应保持 pi 不引入 gpui。
2. **模型配置由外部 `cx-providers` crate 驱动**（`~/.config/cx/cx.providers.config.yaml`），不是 agent 内部代码。pi 侧对应物是「配置 → `Model` + `StreamFn` 的装配层」，可作为 manox 侧适配器而非 pi 本体。
3. **manox 的 Edit 建立在 `hashline/`（2,569 行）之上**：`[path#TAG]` 行号锚定 + 内容哈希校验 + 3-way merge 恢复。pi 的 edit 是常规字符串替换（301 行）。这是语义级分叉，需决策（见 §5）。
4. **prompt 体系是 Tera 模板 + 中英双语**（1,679 行 + 模板），工具描述也双语（descriptions/en.json + zh-CN.json）。这是产品级需求，pi 目前只有简单 system_prompt.rs。
5. **消费链干净**：`manox → agent-ui → agent`，无其他 crate 直接依赖 agent。切换只需照顾 agent-ui 一个消费方。

## 1. Provider 层

| 能力 | manox 位置（规模） | pi 状态 | 说明 |
|---|---|---|---|
| Anthropic Messages 形状 | provider/anthropic.rs (1370) | ✅ | pi 已 live 验证；manox 侧多出 beta header、partial tool_use 事件、签名缺失块丢弃等细节，按需补齐 |
| OpenAI Completions 形状 | completions.rs (1190) | ✅ | 已实现（2026-07-29）：无 CompatFlags/URL 嗅探/钳制；ThinkingKind 即 compat 维度（Enabled→`thinking:{type}`+reasoning_effort、Adaptive→reasoning_effort 透传、off→disabled/"none"、None-level→省略字段）；响应侧无条件宽容解析（reasoning_content/reasoning/reasoning_text、usage 双拼写 .or()、is_error→`[error] ` 前缀、include_usage 恒发）；max_tokens 字段名与 DeepSeek 空 reasoning_content 回填按 TS Pi 规则；thinking 历史丢弃 |
| OpenAI Responses 形状 | responses.rs (1135) | ✅ | 已实现（2026-07-29）：reasoning:{effort}（off/None-level→"none"，on→summary:"auto"+include encrypted_content）；thinking 经 Thinking::signature（ResponseReasoningItem 原始 JSON）往返、无 signature 丢弃、跨模型摊平为纯文本；assistant 文本经新增的 Text::signature（`{v:1,id,phase}`）保持条目身份，fallback `msg_pi_*`；tool call id `call_id\|item_id`，fc_ 前缀规则 + 跨模型 id 省略 + 短哈希（cyrb53 精确移植）；孤儿 tool call 合成 "No result provided" 输出；max_output_tokens ≥16、store:false、system 置顶（thinking→developer 否则 system）；usage 减 cached+write；终态事件缺失/裸错误信封（code+message 无 type）均为 MidStream 错误。注意：responses 形状不做 is_error→`[error] ` 折叠（TS Pi 如此，与 completions 形状有意不同）；tool 结果图片不经视觉能力门控（pi 无 input 能力字段，调用方声明正确性） |
| 握手重试 | retry.rs (383) | ✅ | 已实现（2026-07-29）：provider/retry.rs 形状无关装饰器，三形状共用。429/408/5xx（含 520–524/529）+ 连接期传输错误指数退避 ±20% jitter、Retry-After/retry-after-ms 遵从（≤60s 上限）、6 次总尝试、仅握手阶段重试、AgentEvent::Retry 上抛、终态错误经 overflow 分类。架构差异：pi 的 StreamFn 自带 CancellationToken（manox 靠 tx.is_closed 轮询），错误经 Result 返回而非事件转发 |
| 上下文溢出分类 | overflow.rs (163) | ✅ | 已实现（2026-07-29）：provider/overflow.rs，20 种跨厂商溢出子串 + 7 种限流排除、413 恒判溢出；terminal()（握手非 2xx）与 mid_stream()（流内错误文本）两个构造点统一产出 ProviderError::Overflow，三形状已接线。循环层的 compact-retry 路由属 §3 后续项 |
| Prompt caching 策略 | anthropic_cache.rs (493) | ✅ | 已实现（2026-07-29），按 TS Pi 而非 manox 对齐：`CacheRetention{None,Short(默认),Long}` 枚举挂在 AgentContext（Agent 持字段 + setter，对应 TS Pi `Agent.sessionId`/StreamOptions.cacheRetention，SDK 不读环境变量）；Anthropic 三断点 = 各 system 块 + 末个 tool + 末条 user 消息末块（text/image/tool_result），Long→`ttl:"1h"`；Completions `prompt_cache_key` 门控原样照搬 `(base_url.contains("api.openai.com") && retention != None) \|\| retention == Long`，key=session_id 按 Unicode code point 截 64，Long→`prompt_cache_retention:"24h"`；Responses 无 URL 门控（retention!=None→key，Long→"24h"）；compat flags 不移植、取默认值走 TS Pi 分支（supportsLongCacheRetention/supportsCacheControlOnTools=true、supportsExplicitPromptCacheMode=false→不发 prompt_cache_options）。有意放弃 manox 的差异点：MANOX_PROMPT_CACHING 环境变量、官方 URL 嗅探才发 1h/beta header、messages[-2] 断点布局（DashScope 兼容变通）、None/Full/LastBreakpointOnly 策略轴 |
| SSE 解析 + 截断 JSON 修复 | sse.rs (80) | 🟡 | pi 有 SseParser；manox 另有 `fix_streamed_json`（分隔符栈修复流式 JSON），tool_use 组装需要 |
| Thinking 三态 + effort | 散见各 wire + mod.rs `anthropic_supports_effort` | ✅ | pi 刚完成（ThinkingKind + output_config.effort）；manox 的 effort 按模型 id 门控，pi 由调用方声明，职责划分更干净 |
| 模型注册表（热重载） | registry.rs (475) | 🚫(pi 本体) | cx-providers YAML 驱动、`resolve_apikey`（env/keychain/shell）、输出预算钳制。属于装配层，manox 侧适配器承担 |
| 无 failover 设计 | — | ✅ | 两边一致：模型独立，无自动切换 |

## 2. 核心循环（对标 thread.rs 的循环部分）

| 能力 | manox 位置（规模） | pi 状态 | 说明 |
|---|---|---|---|
| 双流式 turn 循环 | thread.rs (10857 总量) | ✅ | pi agent_loop 双循环 + steering/follow-up 已具备 |
| 取消传播 | thread.rs `cancel()` | 🟡 | pi 有 CancellationToken；manox 另有 pending oneshot 清理、leader→worker 级联。**级联清理有意不移植（2026-07-29 拍板）：TS Pi 取消 = 纯 AbortSignal + 批次级 aborted 检查** |
| 空响应/工具未兑现恢复 | thread.rs（5 种 case） | 🚫 | **有意不移植（2026-07-29 拍板）**：TS Pi agent-loop 无恢复 nudge；error/aborted 立即 agent_end |
| 拒绝熔断 | thread.rs `MAX_CONSECUTIVE_TOOL_DENIALS` | 🚫 | **有意不移植（2026-07-29 拍板）**：TS Pi 无拒绝计数/熔断 |
| MaxTokens 截断处理 | thread.rs | 🟡 | pi 目前 fail 掉全部 tool calls；manox 有更细的恢复路径 |
| 子代理轮数上限 | thread.rs `max_turns` | 🟡 | pi 有全局 max_turns；manox 仅子代理受限 + 两次软着陆（先指令后硬停） |
| 事件体系 | ThreadEvent (~40 变体) | 🟡 | pi AgentEvent 已覆盖主生命周期 + Retry；缺 PlanDelta/SubagentProgress/PrefixStability/TokenUsageUpdated 等产品化事件 |

## 3. 上下文管理

| 能力 | manox 位置（规模） | pi 状态 | 说明 |
|---|---|---|---|
| 自动压缩 | compact.rs + thread.rs | 🟡 | 子件 a（token 计量）已落地（2026-07-30，见下行）；子件 b+c（摘要载体+核心压缩）、d（触发路由）待做，对齐 TS Pi compaction.ts |
| 溢出 compact-retry | thread.rs | 🔲 | 单次兜底：overflow 分类 → 压缩 → 重试一次（子件 d） |
| Token 计量 | token_meter.rs | ✅ | 子件 a（2026-07-30）：`Usage.total_tokens` + `Model.max_tokens` 补齐（三 shape 落值：Anthropic/Completions 线边界折叠求和、Responses 取 wire 原值）；`calculate_context_tokens`（total>0 优先否则四类求和）、`estimate_context_tokens`（usage 锚定 + 尾部字符启发）、`estimate_tokens`（按消息形状：user/toolResult/custom=text+image(4800)，assistant=text+thinking+toolCall(name+JSON)，UTF-16 码元 ceil/4）、`should_compact`（i64 阈值减法）逐项对齐 TS Pi compaction.ts |
| 前缀稳定性检测 | prefix_stability.rs | 🔲 | 请求指纹对比，检测 KV cache 破坏源（产品化观测，可后期） |

## 4. 审批与权限

| 能力 | manox 位置（规模） | pi 状态 | 说明 |
|---|---|---|---|
| 项目信任 | trust.rs | ✅ | 两边都有 |
| 审批模式（AutoPilot/Danger 等） | thread.rs + approval.rs | 🔲 | 模式分派：always-allow 会话缓存 / Danger 全放 / AutoPilot 进评审 |
| AutoPilot 侧 LLM 评审 | approval.rs | 🔲(二期) | 独立小模型批量评审工具调用，8s 超时 fail-closed。产品特性，二期（2026-07-29 确认） |
| 交互式工具通道（AskUser 等） | oneshot + ToolCallAuthorization | 🔲 | pi 需定义 runtime-agnostic 的交互回调接口（gpui 边界的关键一环） |
| 兜底拒绝门 | thread.rs run_tool_inner | 🔲 | 未经任何一层放行的工具一律拒绝，防静默提权 |
| Bash 沙箱（seatbelt） | bash.rs | 🔲(后期) | macOS seatbelt 默认、写 confined、网络拒绝、`unsandboxed` 提权需审批。**后期补（2026-07-29）** |

## 5. 工具集

pi 已有 8 件基础工具。manox 侧 25+，分四类：

### 5a. 基础件对等

| 工具 | manox（规模） | pi 状态 | 差距说明 |
|---|---|---|---|
| Read | read_file.rs (162) | ✅ | 已实现（2026-07-29）：hashline tag 铸造 + `[path#TAG]` + `N:TEXT` 编号输出；pi 保留 offset/limit 参数并映射为 LineRange（manox 的 path_selector 行段选择器不搬）；无限定读取封顶 2000 行并给 offset/limit 分页提示；保留 128KB/2000 行字节护栏。读拒绝清单/LSP 预热不搬 |
| Write | write_file.rs (204) | ✅ | 已实现（2026-07-29）：hashline 前缀剥离（粘贴 read 输出自动去 `[path#TAG]` 头与 `N:` 前缀）+ 快照记录（输出尾部回 `[path#TAG]`）+ mutation_queue 同文件写互斥；保留 pi 的 diff 预览。写沙箱限定/file_lock 不搬（pi 用 FileMutationQueue 阻塞锁代替 NOWAIT file_lock） |
| Edit | edit_file.rs (204) + hashline/ (2569) | ✅ | 已实现（2026-07-29）：**决策（2026-07-29）：移植 hashline，放弃 pi 的 string-replace Edit**——patch-only schema（多 `[path#TAG]` 区段，manox 字段文档整体搬入 JSON schema description）；FileMutationQueue 锁跨 read→patch→write 临界区；TAG 匹配走 apply（回前应用 + 边界修复 + 冲突检测），失配走 3-way 快照恢复；persist() 恢复 CRLF/BOM/尾换行；输出 `[{path}#{new_tag}]\n{diff}` 多区段以 `\n---\n` 相连；block 系 op（SWAP.BLK/DEL.BLK/INS.BLK.POST）随括号平衡解析一并移植 |
| Bash | bash.rs (1610) + background_shell.rs (487) | 🟡 | manox 有 brush 持久 shell、seatbelt 沙箱、后台任务、head/tail 过滤、进程组管理。pi bash 仅 120 行 |
| Grep | grep.rs (320) | 🟡 | 自动 hashline 快照已实现（2026-07-29）：命中文件（≤20，按首命中序去重）重读+normalize+record，模型可直接 edit 免重读。拒绝清单/二进制检测/分页未搬 |
| Glob | glob.rs (154) | 🟡 | gitignore 感知、隐藏文件/目录旗标 |
| List | list_directory.rs (98) | 🟡 | 秘密文件省略、上限 |
| WebFetch | web_fetch.rs (229) | 🔲 | 字节上限、重定向限制、截断检测 |
| BashOutput | bash_output.rs (179) | 🔲 | 依赖 background_shell |
| TaskStop | task_stop.rs (83) | 🔲 | 依赖后台任务注册表 |

### 5b. 代理与编排

| 工具 | manox（规模） | pi 状态 | 说明 |
|---|---|---|---|
| Agent（子代理） | agent.rs (1384) | 🔲 | MAX_DEPTH=5、独立 Thread/工具集/权限缓存、授权冒泡（复合 id）、worktree 隔离选项。**切换必需** |
| EnterWorktree/ExitWorktree | worktree.rs (503) | 🔲 | git worktree 生命周期 + 工具注册表按沙箱重建 |
| Skill | skill.rs (91) | 🔲 | 依赖 skill 注册表（轻） |
| ToolSearch（BM25 懒加载） | tool_search.rs (537) | 🚫 | 不再支持（精简 tools 方向，2026-07-29 决策） |
| Code（QuickJS 编排） | code.rs (713) | 🚫 | 不再支持（精简 tools 方向，2026-07-29 决策） |
| team 工具组（TaskCreate/SendMessage/TeamCreate…） | team/tools.rs (882) | 🔲(二期) | 多智能体协作，见 §7 |

### 5c. 产品/UI 绑定（gpui 边界）

| 工具 | manox（规模） | pi 状态 | 说明 |
|---|---|---|---|
| AskUserQuestion | ask_user.rs (259) | 🚫(暂不) | 暂不实现（2026-07-29）；未来若需要，pi 只出 runtime-agnostic 交互回调接口，gpui 实现在 agent-ui |
| UpdatePlan | update_plan.rs (139) | 🔲 | Context Rail 展示；pi 只需事件通道 |
| GetGoal/CreateGoal/UpdateGoal | goal.rs (183) | 🔲(二期) | 自主目标 + token 预算会计，依赖 db。二期（2026-07-29 确认） |
| Monitor（命令/WS 监控） | monitor.rs (706) + websocket.rs (392) | ❓ | WS 安全校验完备但重。建议二期 |
| SelfInfo | self_info.rs (182) | 🚫 | 不再支持（2026-07-29 决策） |
| web_explore 10 件（浏览器） | web_explore/ (688) | 🚫(pi 本体) | 全部依赖 gpui webview_host，属 UI 层集成，不进 pi |

### 5d. 工具基础设施

| 能力 | manox（规模） | pi 状态 | 说明 |
|---|---|---|---|
| 全局输出截断（50KiB/500B 每行，head+tail） | truncate.rs (188) | 🟡 | pi 有 truncate.rs + output_accumulator；manox 在循环层统一截断，策略更系统 |
| file_lock（NOWAIT try-lock） | file_lock.rs (150) | 🟡 | pi 有 file_mutation_queue（串行化）；manox 是跨代理互斥 + 持有者可见 |
| 双语工具描述 | descriptions.rs + en/zh-CN.json | 🔲 | 产品级需求 |
| schema 生成规整 | tools/mod.rs | 🟡 | schemars→严格端点兼容（去 $schema/$defs、内联 $ref） |

## 6. 持久化与会话

| 能力 | manox（规模） | pi 状态 | 说明 |
|---|---|---|---|
| 会话存储抽象 | — | ✅ | pi session trait + jsonl 实现，runtime-agnostic |
| SQLite 线程持久化 | db/ (2216)：threads/thread_data(zstd BLOB)/events/goals/token_usage/terminals/ui_notes/projects | 🚫 | **决策（2026-07-29）：不移植 SQLite 方案，沿用 pi 风格 jsonl sessions**。zstd BLOB/revision 防竞态等机制随之放弃，jsonl 缺失的能力（按消息 token 索引、ui_notes）届时在 jsonl 上补或在 manox 侧适配 |
| 会话事件审计流 | db/events.rs | ❓ | Goal/压缩/模型变更审计，二期 |

## 7. 子系统

| 子系统 | manox（规模） | pi 状态 | 说明 |
|---|---|---|---|
| Prompt 体系（Tera + 双语 22 模板） | prompt/ (1679) | 🔲(后期) | 严格边界：业务侧只构造类型化数据，渲染集中在模板层。**后期补（2026-07-29）** |
| CLAUDE.md 注入 | turn_ext/ (294) | 🔲 | ContextInjector：固定槽位、字节稳定（保前缀缓存）。turn_ext 另有 CompactionPolicy/ToolGate 钩子（未接线，可不搬） |
| MCP 客户端 | mcp/ (778) | 🔲(后期) | rmcp：stdio + streamable HTTP，插件命名空间合并，工具包装为 mcp_<server>_<tool>。**pi 里没有的暂不加（2026-07-29），切换入场时再补** |
| LSP 工具组（9 件） | lsp/ (897) + 独立 lsp crate | 🔲(二期) | 独立 crate 可直接复用；含读预热、编辑后诊断钩子 |
| 子代理定义（agents/*.md） | agents/ + agent_def | 🔲 | 随 Agent 工具一并移植 |
| Slash commands | command.rs (374) | 🔲(后期) | frontmatter + $ARGUMENTS 渲染 + allowed-tools 门控。**后期补（2026-07-29）** |
| team 多智能体 | team/ (1758) | 🔲(二期) | TaskList 实体 + 点对点消息 + 授权冒泡，建立在子代理之上（2026-07-29 确认二期） |
| hashline | hashline/ (2569) | ✅ | 已移植（2026-07-29）：hash/block/parser/apply/recovery/snapshot 六模块逐行移植 + 集成测试（tempfile 化，剥 gpui 提及）；全局 OnceLock 快照存储改为注入式——`tool::ToolState { snapshots: Mutex<SnapshotStore>, mutation_queue: FileMutationQueue }` 经 `ToolContext::tool_state()` 下发；LineRange 内置于 hashline（manox 的 path_selector 不搬）；xxh32 低 16 位 4-hex tag |

## 8. 成熟判据（MVP 切换集）

**当前阶段聚焦**（pi 已有基础的深化 + 已拍板的移植）：

- [x] Provider：Completions + Responses 两形状补齐（2026-07-29）；retry + overflow 分类（2026-07-29）；prompt caching 策略对齐 TS Pi（2026-07-29）
- [x] hashline 移植（read/write/edit/grep 配套，2026-07-29 完成）
- [x] ~~循环：恢复 nudge、拒绝熔断、取消级联清理~~ 有意不移植（2026-07-29 拍板：TS Pi agent-loop 三项皆无，属 manox 自创）
- [ ] 上下文：自动压缩（子件 a token 计量 ✅ 2026-07-30；b+c 摘要载体与核心压缩、d 触发路由待做）+ 溢出 compact-retry（子件 d）
- [ ] 工具：5a 其余项（Bash 持久 shell/后台、BashOutput/TaskStop/WebFetch）、Agent 子代理、worktree、Skill
- [ ] 工具基础设施：循环层统一截断、schema 规整
- [ ] 持久化：jsonl sessions 扩展（token 计量落盘、线程恢复）
- [ ] 事件体系：补齐 agent-ui 消费所需的事件变体
- [ ] gpui 边界设计文档（事件订阅、线程生命周期对接）

**后期补**（pi 里没有的暂不加，2026-07-29 决策；切换入场前完成）：MCP 客户端、slash commands、Bash seatbelt 沙箱、双语 Tera prompt 体系、CLAUDE.md 注入、审批模式分派 + 兜底拒绝门、双语工具描述

**二期**：team、LSP 工具组、Goal、Monitor/WS、前缀稳定性观测、会话事件审计、AutoPilot 评审

**明确不搬（🚫）**：web_explore 进 pi 本体（属 UI 层）、provider registry 进 pi 本体（属装配层）、SelfInfo、ToolSearch、Code(QuickJS)、AskUserQuestion（暂不）、SQLite 持久化（用 jsonl）、turn_ext 未接线钩子
