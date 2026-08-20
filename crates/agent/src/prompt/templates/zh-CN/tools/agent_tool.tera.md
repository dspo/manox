生成一个子 agent 来处理聚焦的子任务。子 agent 在自己全新的上下文中运行（无父级历史），拥有受限的工具集和专门的系统提示词。适用于：探索代码、调研、并行子任务，或任何会使主上下文膨胀的工作。设置 `isolation: "worktree"` 可让子 agent 在独立 git 工作树的临时分支上运行——与父级工作树完全文件系统隔离；干净的工作树（无提交、无未提交改动）在子 agent 完成后自动移除，有改动的工作树会保留并在结果中回报其分支 + 路径。{% if subagents %}

可用的 subagent_type 值：
{% for s in subagents -%}
- {{ s.name }}（{{ s.capability }}）：{{ s.description }}
{% endfor -%}

括号中的 capability 标签显示每个子 agent 的能力及返回方式：
- `read-only` 子 agent（如 Explore）无法写文件或运行 bash，且**同步**运行——工具调用阻塞直到子 agent 完成，其最终回答即工具结果。不要将写入/执行工作委派给它们。
- `write+bash`（及 `write` / `bash`）子 agent（如 Sailor）**异步**运行——工具立即返回 `{"sailor_id": ..., "status": "dispatched", "isolation": ...}` 句柄，**而非**子 agent 的输出。子 agent 在后台运行；完成时其最终摘要作为 peer 消息到达并触发你的下一轮。在此期间继续干别的活——不要轮询结果。用于并行实现/review/build 验证子任务。

每个子 agent 从空白上下文开始，没有父级历史，因此请将子 agent 必须遵守的任何接口契约（确切的函数名、签名、类型）直接写在提示词中。{% else %}

未加载子 agent 定义。在 ~/.manox/agents/ 下添加 Markdown 文件（frontmatter name/description/tools/model + 正文作为系统提示词）并重启。{% endif %}
