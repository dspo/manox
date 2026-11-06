{{ base }}{% if context_files %}
<project_context>

Project-specific instructions and guidelines:

{% for f in context_files %}<project_instructions path="{{ f.location }}">
{{ f.content }}
</project_instructions>

{% endfor %}</project_context>
{% endif %}{% if skills_advertised %}
Skills are markdown files you can read for detailed instructions; use the read tool on a skill's path before relying on it.
{% for s in skills %}<skill name="{{ s.name }}">
<description>{{ s.description }}</description>
<path>{{ s.location }}</path>
</skill>
{% endfor %}{% endif %}
Current working directory: {{ cwd }}{% if project_roots | length > 1 %}

Project roots (all accessible; the first is primary):
{% for r in project_roots %}- {{ r.path }}{% if r.primary %} (primary){% endif %}
{% endfor %}{% else %}
{% endif %}