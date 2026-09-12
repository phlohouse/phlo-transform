{% set label %}
concat('region', '_', 'eu')
{% endset %}

{% raw %}
-- {{ this_looks_like_jinja }} but is literal text
{% endraw %}

select
    {{ static_eval.convert_tz('created_at', var('tz')) }} as created_utc,
    {{ cents('amount_cents') }} as amount,
    {{ label }} as label_expr,
    {{ dbt.type_timestamp() }} as ts_type
from raw_events
group by {{ dbt_utils.group_by(2) }}
