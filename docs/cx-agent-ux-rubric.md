# Cx Agent UX Rubric

每轮对 `cx-agent` TUI 改造后按 1~5 分打分，总分 35 分。

## 评分维度

| 维度 | 1 分 | 3 分 | 5 分 |
| --- | --- | --- | --- |
| 首屏清晰度 | 无状态提示 | 能看懂主要区域 | 一眼可知状态/动作 |
| 模式可感知性 | mode 切换模糊 | 可感知但不稳定 | chat/stream/approval/recovery 非常清楚 |
| 输入顺滑度 | 常用操作不顺手 | 主要路径可用 | draft/retry/clear/commands 都低摩擦 |
| 时间线可扫描性 | 噪声大 | 基本可扫描 | tool/error/reasoning 快速扫读 |
| Tool 可理解性 | 生命周期不清晰 | 主要状态可见 | started/running/finished/failed 全可见 |
| Approval 低摩擦度 | 判断困难 | 可判断但操作慢 | 风险摘要 + once/turn/session 决策清楚 |
| 错误可恢复性 | 出错后无路可走 | 可以重试 | retry/draft/恢复路径稳定且可预期 |

## 场景清单

1. 正常聊天：发送 prompt 并得到响应。
2. 流式响应：中途滚动历史、停止流式。
3. reasoning：默认折叠，按命令切换可见性。
4. read-only tool：自动通过并观察 lifecycle。
5. write/execute + approval：分别验证 allow once / turn / session / deny。
6. provider/tool/stream error：确认恢复提示与下一步动作。
7. 取消与恢复：中断后 `/retry` 与 `/draft` 可用。

## 达标门槛

- `>= 28/35`：可进入 beta
- `>= 31/35`：接近 Copilot CLI 细腻度
- 任一单项 `< 3`：不达标
