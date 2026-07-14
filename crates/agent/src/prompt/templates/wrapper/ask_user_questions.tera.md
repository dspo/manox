{% for qa in answers -%}
Question: {{ qa.question }}
Answer: {{ qa.answer }}

{% endfor -%}
