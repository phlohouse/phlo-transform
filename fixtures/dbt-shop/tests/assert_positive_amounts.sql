select * from {{ ref('orders_incremental') }} where amount < 0
