三个探索 agent 全部成功。本次覆盖的是此前未触及的三个深水区：

## 测试结果

| # | 探索角度 | 产出亮点 |
|---|---|---|
| 1 | **终端栈 + 外部会话** | `Terminal`(alacritty 引擎) → `PtySource` 抽象 → 本地 shell 与外部 agent CLI 走**同一条渲染管线**；完整输入/输出链路（按键→keys 映射→`Terminal::input`→PTY/`writer_loop`；PTY 读→`OscTap`→逐字节 `advance`→`TerminalElement` prepaint/paint）；`ResumeSidecar` 跨进程恢复机制；readiness 分 Marker（本地 shell）与 Heuristic（agent）两条路径，各自的判定条件与超时策略都不同 |
| 2 | **Provider / LLM 流式层** | 三大 StreamFn（anthropic、openai completions、openai responses）的 wire 形状与 usage 折叠细节；`StreamResolver` 可插拔缝隙（run 中途换模型即换协议）；凭据源四类解析的刻意差异；前缀缓存三条不变量：retry 必须字节相同 body、`transform` 必须确定性、thinking signature 必须保真往返 |
| 3 | **会话持久化 / 压缩 / hashline** | `SessionStorage` 树形会话与磁盘 schema 逐字节对齐；JSONL append-only（首条 assistant 消息才落盘）；`find_cut_point_split` 双摘要切点加 `retained_tail` 自包含检查点；hashline 三向内容锚定恢复与括号平衡解析；SQLite 七表加零 schema migration 的激进纪律 |

## 与前一轮的交叉印证

- 三轮探索的符号引用**完全一致**，同一处装配核心被从三个不同角度分别确认。
- 第三次发现一处值得注意的**文档偏差延续**，这正好印证了「偏离上游必须显式注明理由」这条纪律确实在落地。
- 两个 agent 独立确认了凭据源解析的差异，说明该差异是有意为之而非笔误。

## 两轮累计覆盖

仓库已从六个角度被探索。目前尚未深入的方向：编辑工具的具体实现细节、MCP 桥、审批链路内部、压缩双摘要算法、插件与技能系统。
