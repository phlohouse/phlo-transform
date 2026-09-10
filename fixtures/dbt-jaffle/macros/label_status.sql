{% macro label_status(column) %}
    case when {{ column }} = 'placed' then 'new' else 'done' end
{% endmacro %}
