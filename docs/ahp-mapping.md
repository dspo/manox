# AHP 映射表（P0）

> 本文件是 `~/.claude/plans/soft-marinating-diffie.md` 的 P0 交付物：把 v2 的每一份词汇
> 逐项对到 AHP 落点（action type / state 字段 / command / `x-` 扩展 / 丢弃），**无落点的一律
> 标红**，红行清单在文末。
>
> **状态**：进行中。§1（steer 持久化形态）已完成并附证据；§2 的逐项映射表尚未成文——
> 它是 P1 估算的前置。未完成部分不写"待补"以外的占位。

---

## §1 steer 持久化形态（P0 实证，已完成）

计划「Turn 边界与 part 身份 §1」要求先确认：**steer 在 journal 里到底长什么样**，再定后继
turn id 方案。结论如下，三条证据相互印证：

### 结论

1. **steer 不占独立 entry 种类，落盘形态就是一条普通的 user `message` 条目**，与开启 turn 的
   那条 user 消息同形。`SessionTreeEntry` 只有 `Message { .. }` 与 `Compaction { .. }` 两种
   条目族（`crates/manox-harness/src/core/session/mod.rs:26,40`），没有任何 steer 专属变体。
2. **「user 行已落盘」这一事实是 client 从 journal fold 出来的，不是 server 在线上推的**：
   `ThreadEvent::UserRowLanded` 在 `crates/manox-session-core/src/translate.rs:96-99` 被显式注释为
   *client-fold vocabulary: the SERVER never emits it* —— 也就是说该事件对应的持久事实只能是
   journal 里的 `message` 行，client 靠它退掉乐观回声。
3. **`ThreadEvent::SteerInjected { message_id }` 只是直播通知**，在
   `crates/manox-agent/src/thread.rs:1121`（run 收尾时对 `steered` 列表逐条 push）与
   `crates/manox-agent/src/thread.rs:290`（变体定义）可见；它不新增持久形态。

### 由此确定的 P1 约束

- **直播路径必须与 replay 一致**：既然 replay 时一条 user `message` 条目就是「turn 的起点」，
  那么 mid-turn 的 steer 在直播路径上也必须**关闭当前 turn、开启后继 turn**，否则同一段对话
  运行时是一种渲染、reload 之后是另一种（这正是 pi-ahp 踩过、计划要求照抄的判断）。
- **后继 turn id 采用计划建议的 `<rootTurnId>#<n>`**（`n` 为同一 root turn 下的后继序号）：
  与上面的事实相容，且不需要在条目里编码额外身份。
- `TurnFinished{stranded_steer_ids}`（`crates/manox-session-core/src/translate.rs` 同族）是
  「steer 未注入」的结案信号，映射时并入后继 turn 的收尾，不得丢弃。

### 证据缺口（如实记录）

`crates/manox-harness/tests/fixtures/session/*.txt` **没有 steer 场景**（`grep -rl steer` 零命中），
所以本结论建立在条目族穷举（证据 1）+ 事件语义注释（证据 2）+ 变体用法（证据 3）之上，
而非某一行 fixture。P1 期间若要更强的钉子，应新增一个 steer 的 journal fixture
（一条 user 消息插在 turn 中途），这正是计划里「双路径等价测试」要覆盖的形状。

---

## §2 逐项映射表（未完成）

范围（计划 P0 定义）：`JournalWireEvent`（40）、`HostEvent`（11）、projection key（21）、
`ClientCall`(17)/`ClientNote`(27)/`ServerCall`(6)/`ServerNote`(12)、`ThreadEvent`（~45）。

每行三段：**v2 item → AHP 落点 → 落点类别**（AHP 原生 / `x-manox` 扩展 / 🔴 丢弃）。
落点名必须先在 `ahp-types` 0.9.0 里核实存在（`actions.rs` / `state.rs` / `commands.rs` /
`notifications.rs`），核实不到的按丢弃处理并说明。

**红行清单**（无落点项，逐条给一行理由）与**统计表**（各词汇总数 / AHP 原生 / `x-manox` / 丢弃）
在本节完成后补入。
