{% macro prefixed(p, c) %}{{ p }}_{{ c }}{% endmacro %}

{% macro farewell() %}{{ adapter.dispatch('farewell', 'localpkg')() }}{% endmacro %}
{% macro default__farewell() %}'localpkg-default'{% endmacro %}
