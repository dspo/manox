The following instruction files apply to this session, ordered from broadest scope to most specific. Follow them unless they conflict with the user's explicit requests.
{% for s in sources %}
<instructions scope="{{ s.scope }}" path="{{ s.path }}">
{{ s.content }}
</instructions>
{% endfor %}
