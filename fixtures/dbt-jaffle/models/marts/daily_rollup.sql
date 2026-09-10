{{ config(materialized = 'incremental', unique_key = 'day') }}

select
    created_at::date as day,
    count(*) as orders
from {{ ref('stg_orders') }}
{% if is_incremental() %}
where created_at >= (
    select date_trunc('day', max(created_at)) from {{ this }}
)
{% else %}
where created_at >= date '2000-01-01'
{% endif %}
