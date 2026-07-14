{{ base }}{% if language %}

## Language

Unless the user specifies otherwise, write your user-facing responses in {{ language.language }}.
{% endif %}{% if worktree_subagent %}

## Active worktree
You are running inside a git worktree on branch `{{ worktree_subagent.branch }}` at `{{ worktree_subagent.path }}`. Your cwd is this worktree, not the parent's project root. Work here; git operations (commit/push) run without approval. A clean worktree is auto-removed when you finish — commit or keep your work explicitly if it must persist.{% endif %}{% if goal %}
{% include "mode/goal.tera.md" %}{% endif %}{% if ultracode %}
{% include "mode/ultracode.tera.md" %}{% endif %}
