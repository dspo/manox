位于 `{{ plan_file }}` 的 plan 已获用户批准。读取该 plan 文件，然后严格按其内容自上而下实施：

- plan 已决策完备——执行它，不要重新规划、重新设计或重开已定决策。
- 若某步出现 plan 无法预见的歧义，选择与 plan 的 Context 和 Verification 小节一致的最小解释，并在最终总结中说明该选择。
- 按 plan 的 Verification 小节要求验证每个关键步骤，然后再报告完成。
- 用 `UpdatePlan` 发布并跟踪执行进度：开始执行后立即发布完整步骤列表，此后每次进度变化都更新（完成即标记 completed，至多一个 in_progress，结束前全部 completed）。它驱动展示给用户的 plan 概览。
