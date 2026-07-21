<system-reminder>
以下指令文件是在读取上述文件时发现的，从此处起生效。
{% for s in sources %}
<instructions scope="{{ s.scope }}" path="{{ s.path }}">
{{ s.content }}
</instructions>
{% endfor %}
</system-reminder>
