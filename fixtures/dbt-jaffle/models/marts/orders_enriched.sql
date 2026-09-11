select
    {{ dbt_utils.generate_surrogate_key(['order_id', 'status']) }} as order_key,
    {{ cents_to_dollars('amount') }} as amount_usd
from {{ ref('orders_incremental') }}
