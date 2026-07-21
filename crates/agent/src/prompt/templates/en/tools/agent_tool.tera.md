Spawn a sub-agent to handle a focused subtask. The sub-agent runs in its own fresh context (no parent history), with a restricted tool set and a specialized system prompt. Only its final assistant message returns as the tool result. Useful for: exploring code, research, parallel subtasks, or any work that would bloat the main context. Set `isolation: "worktree"` to run the sub-agent in its own git worktree on a fresh branch — full filesystem isolation from the parent's working tree (the child cannot write the parent's project root); a clean worktree is auto-removed when the sub-agent finishes.{% if subagents %}

Available subagent_type values:
{% for s in subagents -%}
- {{ s.name }} ({{ s.capability }}): {{ s.description }}
{% endfor -%}

The capability tag in parentheses shows what each sub-agent can do: `read-only` sub-agents cannot write files or run bash — do not delegate write/exec work to them (they will refuse and waste a round). Each sub-agent starts from a blank context with no parent history, so pin any interface contract the sub-agent must honor (exact function names, signatures, types) directly in the prompt.{% else %}

No sub-agent definitions are loaded. Add Markdown files under ~/.config/cx/manox/agents/ (frontmatter name/description/tools/model + body as system prompt) and restart.{% endif %}
