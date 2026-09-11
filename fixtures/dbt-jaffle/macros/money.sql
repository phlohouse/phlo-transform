{% macro cents_to_dollars(column_name, scale=2) %}
    {{ return(adapter.dispatch('cents_to_dollars')(column_name)) }}
{% endmacro %}

{% macro default__cents_to_dollars(column_name, scale=2) %}
    ({{ column_name }} / 100)::numeric(16, 2)
{% endmacro %}
