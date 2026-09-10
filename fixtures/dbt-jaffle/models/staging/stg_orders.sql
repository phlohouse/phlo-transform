select
    id as order_id,
    customer_id,
    amount,
    status,
    created_at
from {{ source('raw', 'orders') }}
where region = '{{ var('region') }}'
