# Spike：自研 AHP host 与官方 `ahp::hosts` 的收敛评估（#877）

**结论：止步——收敛对象不存在。** `ahp::hosts` 不是宿主侧框架，是**多宿主客户端
SDK**；manox 手搓的宿主半边没有可收敛的官方对应物。这与 ahp-v3 架构文档 §E.1
立项时的判断（「必须自建（AHP 无宿主 SDK）」）一致，本 spike 以 ahp 1.0.0 源码
复核了该判断仍然成立。

## 第零问（在三问之前）：`ahp::hosts` 是什么

模块导言（`ahp-1.0.0/src/hosts/mod.rs`）：

> Multi-host **client** SDK. A consumer that wants to talk to two or more AHP
> hosts at once would otherwise have to hand-roll N independent `Client`s, N
> transports, N reconnect supervisors…

全部成员都是客户端侧机械：

- `HostConfig`——怎么**连**一个宿主（传输工厂、重连策略、`clientId`、初始订阅）；
- `HostHandle`——UI 渲染的连接状态快照；
- `hosts/runtime.rs`——「Per-host supervisor task. Owns the current `Client`,
  the reconnect state machine」；
- `client_id_store.rs`——跨启动的 `clientId` 持久化（客户端身份）。

三问因此失去对象：

1. **journal 权威折叠源**：官方框架不提供任何宿主侧状态机或折叠源——无可挂载。
2. **x-manox ext 通道承载**：宿主侧通道机械（router / sequencer / channels）
   全在 manox-ahp，官方无对应物。
3. **`mcp://` 原始转发 side-channel**：同上。

## 附带发现（转介，非 manox 的活）

- `MultiHostStateMirror`（`ahp::multi_host_state_mirror`）：给「同时连多宿主的
  客户端」的 reducer 门面——按 `(host_id, uri)` 键状态、跨宿主 URI 冲突安全。
  这是 **dspo/manox-app 的礼物**而非 manox 的：若 app 将来同时连多个宿主
  （本地 + 远程），它是现成的状态镜像层，值得在该场景出现时评估。
- 官方 SDK 内唯一的宿主侧资产仍是 reducers（双端同码 fold）——manox 已在用，
  且 #879 之后通道状态就是它们的直折产物。

## 后续

- manox-ahp 的宿主半边（jsonrpc/peer、router、sequencer、transport/inproc）
  继续自持；升级税经 #879 的单投影收敛已显著降低（SDK 升级的适配面 ≈ 类型层
  字面量）。
- 若未来 SDK 发布真正的宿主框架，重开本评估（三问原文见 #877）。
