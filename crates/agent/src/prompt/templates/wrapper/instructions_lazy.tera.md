<system-reminder>
The following instruction files were discovered while reading the file above and apply from this point on.
{% for s in sources %}
<instructions scope="{{ s.scope }}" path="{{ s.path }}">
{{ s.content }}
</instructions>
{% endfor %}
</system-reminder>
