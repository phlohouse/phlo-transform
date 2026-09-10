select
    order_id,
    {{ label_status('status') }} as status_label
from {{ ref('stg_orders') }}
