以下指令文件适用于本会话，按范围从最宽到最窄排列。除非与用户的明确请求冲突，否则请遵循它们。
{% for s in sources %}
<instructions scope="{{ s.scope }}" path="{{ s.path }}">
{{ s.content }}
</instructions>
{% endfor %}
