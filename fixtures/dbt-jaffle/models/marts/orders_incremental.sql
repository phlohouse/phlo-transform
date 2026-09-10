{{ config(materialized = 'incremental', unique_key = 'order_id', incremental_strategy = 'merge') }}

select
    order_id,
    customer_id,
    amount,
    created_at
from {{ ref('stg_orders') }}
{% if is_incremental() %}
where created_at > (select max(created_at) from {{ this }})
{% endif %}
