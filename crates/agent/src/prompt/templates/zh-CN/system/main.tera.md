{{ static_body }}{% if skills %}

## 可用技能（通过 `skill` 工具按需查阅完整内容）
{% for s in skills -%}
- {{ s.name }}：{{ s.description }}
{% endfor %}{% endif %}

## 语言

除非用户另有指定，否则用{{ language.language }}撰写面向用户的回复。

## 运行时身份

- 当前工作目录：`{{ runtime.cwd }}`
{% if runtime.project -%}
- 项目根目录：`{{ runtime.project }}`
{% endif -%}
{% if runtime.active_worktree -%}
- 活跃工作树：`{{ runtime.active_worktree.branch }}`，位于 `{{ runtime.active_worktree.path }}`
{% endif -%}
- 操作系统：{{ runtime.os }}
- 默认 shell：{{ runtime.shell }}
- python3：{{ runtime.python3 }}
- node：{{ runtime.node }}
- 今天：{{ runtime.today }}
{% if runtime.approval_mode == "Danger" -%}
- 模式：危险驾驶（工具调用无需审批，bash 在沙箱外运行）
{% endif -%}
