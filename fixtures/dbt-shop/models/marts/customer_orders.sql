{{ config(materialized = 'table') }}

select
    c.customer_id,
    c.name,
    count(o.order_id) as order_count,
    coalesce(sum(o.amount), 0) as total_amount
from {{ ref('stg_customers') }} c
left join {{ ref('stg_orders') }} o
    on o.customer_id = c.customer_id
group by c.customer_id, c.name
