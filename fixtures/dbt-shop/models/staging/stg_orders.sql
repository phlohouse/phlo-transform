select
    id as order_id,
    customer_id,
    amount,
    status,
    ordered_at
from {{ source('raw', 'orders') }}
