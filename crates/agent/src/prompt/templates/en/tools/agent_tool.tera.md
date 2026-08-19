Spawn a sub-agent to handle a focused subtask. The sub-agent runs in its own fresh context (no parent history), with a restricted tool set and a specialized system prompt. Useful for: exploring code, research, parallel subtasks, or any work that would bloat the main context. Set `isolation: "worktree"` to run the sub-agent in its own git worktree on a throwaway branch — full filesystem isolation from the parent's working tree; a pristine worktree (no commits, no uncommitted changes) is auto-removed when the sub-agent finishes, and a worktree with work is kept with its branch + path reported back in the result.{% if subagents %}

Available subagent_type values:
{% for s in subagents -%}
- {{ s.name }} ({{ s.capability }}): {{ s.description }}
{% endfor -%}

The capability tag in parentheses shows what each sub-agent can do and how it returns:
- `read-only` sub-agents (e.g. Explore) cannot write files or run bash, and run **synchronously** — the tool call blocks until the sub-agent finishes and its final answer is the tool result. Do not delegate write/exec work to them.
- `write+bash` (and `write` / `bash`) sub-agents (e.g. Sailor) run **asynchronously** — the tool returns immediately with a `{"sailor_id": ..., "status": "dispatched", "isolation": ...}` handle, NOT the sub-agent's output. The sub-agent runs in the background; when it settles, its final summary arrives as a peer message and triggers your next turn. Continue with other work in the meantime — do not poll for the result. Use this for parallel implementation/review/build-verification subtasks.

Each sub-agent starts from a blank context with no parent history, so pin any interface contract the sub-agent must honor (exact function names, signatures, types) directly in the prompt.{% else %}

No sub-agent definitions are loaded. Add Markdown files under ~/.manox/agents/ (frontmatter name/description/tools/model + body as system prompt) and restart.{% endif %}
