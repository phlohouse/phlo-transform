select
    id as customer_id,
    name,
    '{{ var('region') }}' as region
from {{ source('raw', 'customers') }}
