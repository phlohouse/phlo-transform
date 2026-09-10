select
    id as customer_id,
    name,
    coalesce(region, '{{ var('default_region') }}') as region
from {{ source('raw', 'customers') }}
