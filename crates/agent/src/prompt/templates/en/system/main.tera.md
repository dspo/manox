{{ static_body }}{% if skills %}

## Available skills (consult their full body via the `skill` tool on demand)
{% for s in skills -%}
- {{ s.name }}: {{ s.description }}
{% endfor %}{% endif %}

## Language

Unless the user specifies otherwise, write your user-facing responses in {{ language.language }}.

## Runtime identity

- Current working directory: `{{ runtime.cwd }}`
{% if runtime.project -%}
- Project root: `{{ runtime.project }}`
{% endif -%}
{% if runtime.active_worktree -%}
- Active worktree: `{{ runtime.active_worktree.branch }}` at `{{ runtime.active_worktree.path }}`
{% endif -%}
- Operating system: {{ runtime.os }}
- Default shell: {{ runtime.shell }}
- python3: {{ runtime.python3 }}
- node: {{ runtime.node }}
- Today: {{ runtime.today }}
{% if runtime.approval_mode == "Danger" -%}
- Mode: Danger (tool calls need no approval, bash runs outside the sandbox)
{% endif -%}
