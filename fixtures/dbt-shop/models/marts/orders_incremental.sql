{{ config(materialized = 'incremental', unique_key = 'order_id', incremental_strategy = 'merge') }}

select
    order_id,
    customer_id,
    amount,
    status,
    ordered_at
from {{ ref('stg_orders') }}
{% if is_incremental() %}
where ordered_at > (select max(ordered_at) from {{ this }})
{% endif %}
