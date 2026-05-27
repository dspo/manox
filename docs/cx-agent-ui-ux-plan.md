# Cx Agent UI / 交互打磨计划（对标 Copilot CLI）

> **目标**：把 `cx-agent` 从“功能完整的 v0”打磨成“终端内细腻、稳定、可恢复、键盘优先”的交互产品。
>
> **标杆**：GitHub Copilot CLI 当前体现出来的体验特征——明确的模式切换、持续可感知的状态反馈、丰富但一致的快捷键、低摩擦的中断/恢复路径、以及“需要时很强，不需要时很安静”的信息密度控制。

---

## 1. 这份计划解决什么问题

当前 `cx-agent` 已经具备：

- launcher 入口
- 全屏 ratatui 聊天界面
- 流式输出
- tool loop
- approval
- rollout / stats

但离 “像 Copilot CLI 一样细腻” 还有明显距离。现状更偏“能用”，下一阶段目标要转成“顺手、可预期、低心智负担”。

这份计划用于：

1. 统一下一轮 UI / 交互打磨的北极星目标。
2. 给实现阶段提供**分阶段、可验收**的路线图。
3. 给后续 `/autoresearch` 提供可直接复用的 metric / scope / constraints。

---

## 2. 默认采用的 autoresearch 设定

本轮先写计划文档，未直接启动 autoresearch loop；由于用户当时不在线，以下参数采用**默认假设**，后续如需正式跑 `/autoresearch`，以本表为初始值再确认即可。

| Parameter | Assumed value |
| --- | --- |
| Goal | 把 Cx Agent 的 UI / 交互打磨到接近 Copilot CLI 的细腻体验，包括首屏选择、聊天区、流式反馈、工具调用反馈、审批交互、错误提示与键盘体验 |
| Metric command | **Phase 0 先补齐度量脚本**：`cargo test && cargo clippy -- -D warnings && ./scripts/ux/cx-agent-smoke.sh` |
| Metric extraction | 以 UX rubric 总分为主（见 §6），同时 smoke / tests / clippy 必须通过 |
| Direction | `higher_is_better` |
| In-scope files | `docs/`, `src/cx_agent/tui/`, `src/cx_agent/session.rs`, `src/cx_agent/approval.rs`, `src/cx_agent/history.rs`, `src/cx_agent/events.rs`, `src/cx_agent/provider/`, `src/lib.rs` |
| Out-of-scope files | 与 Cx Agent 无关的其他产品路径；非必要的 build/release 流程；stats 主体信息架构（仅允许为 Cx Agent UX 联动做小改） |
| Constraints | 不引入新的重量级依赖；保持 provider/tool 功能正确；不破坏现有测试；优先复用现有 TUI 架构；同等收益下优先更简单方案 |
| Max experiments | `10`（用于后续真实 autoresearch 时的首轮预算） |
| Simplicity policy | 接受默认：小收益不值得引入明显复杂度 |

---

## 3. 当前实现 vs. Copilot CLI：关键体验差距

### 3.1 外壳（chrome）与状态表达还不够细

当前 `cx-agent` 已有：

- alt-screen
- banner / footer
- status line
- chat / streaming / approval 三种 mode

但仍然缺少 Copilot CLI 那种“状态永远清楚”的外壳层次：

- 顶部/底部信息层级还不够稳定
- mode 切换时缺少明显的视觉节奏
- 只有单条 `status_note`，缺少**短时事件**与**持久状态**的分层
- 没有像 Copilot CLI 那样明确的“当前可用动作提示”

### 3.2 对话时间线还偏“日志感”，不够“交互感”

当前 tool / reasoning / usage / error 都能显示，但更接近线性日志：

- tool 调用反馈是系统消息串，不像“有状态的卡片”
- reasoning 只做了状态切换，没有折叠/展开/隐藏策略
- assistant streaming 缺少更稳定的增量视觉反馈
- 缺少 turn 级分组、摘要、重试入口

Copilot CLI 的强项不是信息更多，而是“什么时候展开，什么时候克制”。

### 3.3 输入体验仍是 v0 水平

当前输入框支持：

- `Enter` 发送
- `Alt-Enter` 换行
- `PgUp/PgDn` 滚动
- `Esc/Ctrl-C` 退出

但和 Copilot CLI 相比仍缺：

- 更完整的行内编辑与光标反馈
- prompt 草稿保留 / 恢复
- slash command / 内联帮助
- 更好的中断、取消、重试和继续上一次操作
- 更强的一致性快捷键设计

### 3.4 Approval 交互已经有骨架，但还不够“低摩擦”

当前 approval prompt 已支持：

- tool category
- 参数预览
- allow / deny
- 滚动浏览

仍缺少更高质量的微交互：

- 关键信息摘要（本次会写什么/执行什么）
- 更紧凑的参数差异预览
- allow once / allow for turn / allow for session 之类的层级策略
- deny 后的恢复路径提示
- 批准前后的上下文连续性

### 3.5 错误恢复和可逆性还不够强

当前错误会显示，但对用户来说还不够“可操作”：

- stream/provider/tool 失败缺少分类文案
- 缺少 retry / edit-and-resend / resume-last-input
- 中断后可恢复路径不够明显
- 没有显式区分“可忽略问题”和“需要退出的问题”

---

## 4. 下一阶段的设计原则

所有 UI / 交互打磨都遵循这 6 条原则：

1. **Quiet by default**：默认安静，详细信息按需展开。
2. **State is always obvious**：用户必须一眼知道现在是在输入、流式响应、审批、还是错误恢复。
3. **Keyboard-first**：所有关键路径都必须能纯键盘完成，而且快捷键语义稳定。
4. **Every interruption is recoverable**：取消、deny、失败后都要给出下一步。
5. **Events are transient, status is persistent**：短时事件和长期状态分层显示。
6. **Microcopy matters**：提示文案要短、准、可执行，不要解释型废话。

---

## 5. 实施路线图

### Phase 0 — 建立 UX 基线与度量

**目标**：先把“什么叫更好”量化，否则后续只会陷入主观争论。

**要做的事**

- 新增 `docs/` 下的 UX rubric 评分卡（可并入本文件，或拆成附录）
- 新增 scripted smoke（建议 `scripts/ux/cx-agent-smoke.sh`）
- 定义 5～8 个标准场景：
  - 正常聊天
  - 流式响应
  - reasoning 输出
  - read-only tool
  - write/execute tool + approval
  - provider / tool / stream error
  - 取消 / 退出 / 恢复
- 确定每个场景的可观察输出

**主要文件**

- `docs/cx-agent-ui-ux-plan.md`
- `scripts/ux/cx-agent-smoke.sh`（后续新增）
- `src/cx_agent/tui/chat.rs`

**退出条件**

- 有一套可以重复执行的 smoke 场景
- 有一张可打分的 rubric
- 有 baseline 分数

---

### Phase 1 — 打磨 shell chrome 与信息层级

**目标**：先把“看起来是否稳、状态是否清楚”做对。

**要做的事**

- 重构顶部 / 底部信息层级：
  - 会话身份（provider / model / wire_api）
  - mode badge（chat / streaming / approval / error-recovery）
  - usage 与 session 状态分层
- 区分：
  - **persistent status**（当前模式、累计 usage、session id）
  - **ephemeral event**（tool started、tool finished、stream completed）
- 优化 footer 文案，只显示**当前 mode 真正可用的动作**
- 明确 focus 与滚动状态（例如是否跟随输出）

**主要文件**

- `src/cx_agent/tui/chat.rs`
- `src/cx_agent/events.rs`

**退出条件**

- 用户在任何时刻都能一眼看懂当前状态
- footer 提示不再堆叠无关快捷键
- streaming / approval / error 的视觉状态切换清楚

---

### Phase 2 — 重做时间线表达：从“日志”到“交互”

**目标**：让对话区更像产品界面，而不是原始日志滚动。

**要做的事**

- 把 tool call / tool result 变成结构化块
- 给 reasoning 增加折叠策略：
  - 默认折叠
  - 快捷键展开/收起
  - 只显示摘要提示
- turn 粒度做分组：
  - user input
  - assistant streaming / final
  - tool group
  - usage meta
- 为错误、拒绝审批、取消操作引入更清晰的视觉语义

**主要文件**

- `src/cx_agent/tui/chat.rs`
- `src/cx_agent/history.rs`
- `src/cx_agent/session.rs`

**退出条件**

- tool / reasoning / error 都能被快速扫描
- 时间线不再充满“sys>”噪声
- turn 与 turn 之间更易区分

---

### Phase 3 — 输入与快捷键体验对齐 Copilot CLI

**目标**：把“能输入”升级成“输入很顺手”。

**要做的事**

- 系统整理键位，形成统一表
- 增强输入框：
  - draft 保留
  - resend last prompt
  - clear input / edit in place
  - 更明显的 cursor / selection / multiline 行为
- 设计轻量 slash commands：
  - `/help`
  - `/quit`
  - `/clear`
  - `/retry`
  - `/usage`
  - `/tools`
- 评估是否支持类似 Copilot CLI 的“更细粒度编辑快捷键提示”

**主要文件**

- `src/cx_agent/tui/chat.rs`
- `src/cx_agent/session.rs`

**退出条件**

- 常见操作不需要离开键盘
- 至少有一套稳定、记忆成本低的命令式交互
- 新用户首次进入时能发现这些能力

---

### Phase 4 — Approval / tool 交互做成“可判断、可恢复、可继续”

**目标**：把审批链路打磨成高信任体验。

**要做的事**

- approval 卡片增加“本次风险摘要”
- 对 write / execute 采用不同的视觉强调
- 评估细化授权策略：
  - allow once
  - allow for turn
  - allow for session
- deny 后增加后续动作提示：
  - 修改请求
  - 改用 read-only tool
  - 解释原因继续
- 统一 tool started / running / finished / failed 的文案模板

**主要文件**

- `src/cx_agent/tui/approval_prompt.rs`
- `src/cx_agent/tui/chat.rs`
- `src/cx_agent/approval.rs`
- `src/cx_agent/session.rs`

**退出条件**

- 用户能快速理解“为什么现在要我决定”
- deny 之后不会让会话显得“断掉”
- tool 生命周期可观察、可理解

---

### Phase 5 — 错误恢复、取消与连续性

**目标**：把失败做成“可恢复的偏航”，而不是“交互终止”。

**要做的事**

- 区分 error class：
  - provider/network
  - tool execution
  - approval denied
  - malformed tool output
  - user cancel
- 为每种错误定义 UI 响应：
  - 文案
  - 高亮
  - 下一步动作
- 增加：
  - retry last turn
  - resume draft
  - cancel streaming but keep transcript
- 让退出语义更一致：退出应用、结束本轮、取消审批要清晰分开

**主要文件**

- `src/cx_agent/session.rs`
- `src/cx_agent/tui/chat.rs`
- `src/cx_agent/provider/rig.rs`

**退出条件**

- 常见失败路径都能“走回来”
- Ctrl-C / Esc 的行为边界明确
- 错误不会把用户带进死胡同

---

### Phase 6 — 最终 polish + 回归

**目标**：把所有零散改动压成统一体验。

**要做的事**

- 清理 microcopy
- 统一颜色 / 间距 / 边框 / badge 语义
- 复跑完整 smoke
- 依据 rubric 打最终分
- 按真实使用反馈再做一轮微调

**退出条件**

- UX rubric 达到目标阈值
- 没有明显割裂感
- smoke / test / clippy 全绿

---

## 6. UX rubric（后续 autoresearch 的主度量）

每轮评估打 1～5 分，总分 35 分。

| 维度 | 说明 |
| --- | --- |
| 首屏清晰度 | 第一次进入时，用户是否马上知道自己在哪、能做什么 |
| 模式可感知性 | chat / streaming / approval / error 的切换是否一眼可见 |
| 输入顺滑度 | 多行输入、重发、取消、快捷键是否自然 |
| 时间线可扫描性 | 对话、tool、reasoning、usage 是否容易扫读 |
| Tool 可理解性 | tool started / running / result / fail 是否好理解 |
| Approval 低摩擦度 | 判断成本是否低，拒绝后是否易恢复 |
| 错误可恢复性 | 出错时是否有明确下一步 |

**建议门槛**

- `>= 28/35`：可进入 beta 打磨
- `>= 31/35`：接近“细腻”
- 任一单项 `< 3`：不允许宣称达标

---

## 7. 推荐的实现顺序

为了避免 `chat.rs` 再次成为冲突泥潭，建议按下面顺序推进：

1. **先做 Phase 0**：度量与 rubric，避免凭感觉优化
2. **再做 Phase 1**：先稳住 shell chrome
3. **再做 Phase 2**：处理时间线与 tool/reasoning 表达
4. **再做 Phase 3/4**：输入与 approval 是高频路径，值得重点磨
5. **最后做 Phase 5/6**：把恢复性和文案统一收口

如果要并行分工，建议只按下面方式拆：

- 一人负责 `chat.rs` 的 rendering / keymap / chrome
- 一人负责 `session.rs` / approval / tool lifecycle
- 一人负责 rubric / smoke / docs

不要让两个人同时大改 `chat.rs` 的同一片区域。

---

## 8. 明确不做的事

这一轮 UI / 交互打磨**不**包含：

- 新的 tool 类型
- MCP 客户端
- 多 agent / background agent 系统
- Web UI / GUI 化
- 为了 UI polish 引入重量级新依赖
- 改写整个 stats 或 launcher 架构

---

## 9. 本轮计划的验收口径

当以下条件满足时，可以认为“对标 Copilot CLI 的第一轮 UI/交互打磨计划”已完成并可开始实施：

- 计划文件已明确目标、度量、范围、约束与阶段路线图
- UX rubric 已定义
- implementation order 已明确
- 后续 `/autoresearch` 可以直接拿本文件作为 setup 初稿
- 文档已随 PR 提交

---

## 10. 下一步（紧接着本计划之后）

1. 补 `scripts/ux/cx-agent-smoke.sh`
2. 建立 baseline rubric 分数
3. 从 **Phase 1 — shell chrome 与信息层级** 开始实施
4. 每完成一个 phase，都回写本文件的“当前进展 / 基线分数 / 已验证体验差异”
