{{ config(materialized = 'table', tags = ['gold']) }}

select
    c.customer_id,
    c.name,
    count(o.order_id) as order_count
from {{ ref('stg_customers') }} c
left join {{ ref('stg_orders') }} o
    on o.customer_id = c.customer_id
group by c.customer_id, c.name
