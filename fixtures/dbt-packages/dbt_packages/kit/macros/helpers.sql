{% macro squared(x) %}
  ({{ x }} * {{ x }})
{% endmacro %}

{% macro unroll(cols) %}
  {% for c in cols %}{{ c }}{% if not loop.last %}, {% endif %}{% endfor %}
{% endmacro %}

{% macro signature() %}
  'kit-v1'
{% endmacro %}

{% macro dynamic() %}
  {{ run_query('select 1') }}
{% endmacro %}

{% macro greet() %}{{ adapter.dispatch('greet', 'kit')() }}{% endmacro %}
{% macro default__greet() %}'kit-default'{% endmacro %}
