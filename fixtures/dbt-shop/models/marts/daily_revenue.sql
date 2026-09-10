{{ config(materialized = 'incremental', incremental_strategy = 'append') }}

select
    ordered_at,
    status,
    sum(amount) as revenue
from {{ ref('stg_orders') }}
{% if is_incremental() %}
where ordered_at > (select max(ordered_at) from {{ this }})
{% endif %}
group by ordered_at, status
