生成一个子 agent 来处理聚焦的子任务。子 agent 在自己全新的上下文中运行（无父级历史），拥有受限的工具集和专门的系统提示词。只有其最终助手消息作为工具结果返回。适用于：探索代码、调研、并行子任务，或任何会使主上下文膨胀的工作。设置 `isolation: "worktree"` 可让子 agent 在独立 git 工作树的新分支上运行——与父级工作树完全文件系统隔离（子 agent 无法写入父级的项目根目录）；干净的工作树在子 agent 完成后自动移除。{% if subagents %}

可用的 subagent_type 值：
{% for s in subagents -%}
- {{ s.name }}（{{ s.capability }}）：{{ s.description }}
{% endfor -%}

括号中的 capability 标签显示每个子 agent 的能力：`read-only` 子 agent 无法写文件或运行 bash——不要将写入/执行工作委派给它们（它们会拒绝并浪费一轮）。每个子 agent 从空白上下文开始，没有父级历史，因此请将子 agent 必须遵守的任何接口契约（确切的函数名、签名、类型）直接写在提示词中。{% else %}

未加载子 agent 定义。在 ~/.config/cx/manox/agents/ 下添加 Markdown 文件（frontmatter name/description/tools/model + 正文作为系统提示词）并重启。{% endif %}
