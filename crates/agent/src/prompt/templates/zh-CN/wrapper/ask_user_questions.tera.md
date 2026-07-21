{% for qa in answers -%}
问题：{{ qa.question }}
回答：{{ qa.answer }}

{% endfor -%}
{% if response -%}
补充说明：{{ response }}
{% endif -%}
