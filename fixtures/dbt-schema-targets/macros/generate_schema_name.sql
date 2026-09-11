{% macro generate_schema_name(custom_schema_name, node) %}

    {% set default_schema = target.schema %}

    {% if node.resource_type == 'seed' %}
        {{ custom_schema_name | trim }}

    {% elif custom_schema_name is none %}
        {{ default_schema }}

    {% elif target.name == 'prod' %}
        {{ default_schema }}_{{ custom_schema_name | trim }}

    {% else %}
        {{ custom_schema_name | trim }}
    {% endif %}

{% endmacro %}
