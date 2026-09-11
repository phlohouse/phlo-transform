{% macro boolean_var(name, default=false) %}
    {% set value = var(name, default) %}
    {% if value is not sameas true and value is not sameas false %}
        {% do exceptions.raise_compiler_error("var must be boolean") %}
    {% endif %}
    {% do return(value) %}
{% endmacro %}

{% macro convert_tz(column, target_tz) %}
{%- set tz = "UTC" if not target_tz else target_tz -%}
{{ return(adapter.dispatch('convert_tz', 'static_eval') (column, tz)) }}
{% endmacro %}

{% macro default__convert_tz(column, tz) -%}
convert_timezone('UTC', '{{ tz }}', {{ column }})
{%- endmacro %}

{% macro cents(amount) %}
    {% do log("converting cents", info=true) %}
    {% do return('(' ~ amount ~ ' / 100)::numeric(16, 2)') %}
{% endmacro %}
