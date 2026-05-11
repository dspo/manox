# Agent CLI Concept Mapping: Copilot / Codex / Claude

简要概念（一句话）

- Skill: 可复用的能力或动作模块，供 agent 调用（类似插件）。
- Subagent: 主 agent 调用的子助手，负责独立子任务。
- Agents team: 多个 agent 协作完成复杂任务的编排。
- Plugin: 扩展 CLI/运行时的外部模块或集成。
- MCP: Codex 特有的包/插件管理系统（管理/安装插件）。
- Provider: 模型/服务提供方（DashScope、Azure、Packy 等）。
- Model: 具体模型 ID（如 qwen3.6-plus、claude-opus-4-6）。
- Wire API: 与模型交互的接口类型（completions / responses 等）。
- Probe: 检测模型对不同 wire API 支持并缓存结果的工具。

## 对照表（简洁）

| Concept | Copilot | Codex | Claude | 备注/示例 |
|---|---|---|---|---|
| Skill | 内建动作或扩展能力（用户可触发的 feature） | 可插拔的 skills / 功能模块 | 以 tool/functions 的形式暴露（Packy 中的工具） | 示例：代码搜索、格式化 |
| Subagent | 非常少见；通常靠外部编排或脚本实现 | 支持子 agent / 多进程协作模式 | Packy 工具可作为子 agent 执行特定任务 | 示例：单独的 lint/测试 子 agent |
| Agents team | CLI 层面不常见 | 支持 multi-agent 协作模式 | 通过 Packy/外部编排实现 | 用于复杂工作流分工 |
| Plugin | 编辑器/CLI 插件系统 | MCP 管理的插件/包 | Packy/Anthropic 的工具集成 | 示例：mcp install/插件安装 |
| MCP | n/a | codex 的包/插件管理 (mcp) | n/a | Codex 特有的管理子系统 |
| Provider | GitHub/DashScope（或 env 配置） | Azure / DashScope / 本地代理 | Packy / Anthropic / DashScope | 决定端点与认证方式 |
| Model | 由 COPILOT_MODEL 或配置指定 | 通过 flag/env 指定模型 | 由 ANTHROPIC_MODEL/Packy 映射（如 claude-opus-4-6） | 示例：替换 Packy 默认模型为 claude-opus-4-6 |
| Wire API | completions / responses（可选，通过 probe 确定） | 可配置（某些模型仅支持 completions） | Packy/Anthropic 支持 responses vs completions，需探测 | 影响调用形式与能力 |
| Probe | ccc/cx 的 probe 子命令，缓存模型能力 | 可实现类似探测脚本 | 对 Packy/Anthropic 也适用，决定使用哪种 wire API | 用于自动选择最佳接口 |

简洁说明：如果要把这张表加入仓库中，已写入 docs/agents-comparison.md 。如需更详细示例或把内容合入 README，请告知。
