select
    id,
    kind,
    occurred_at
from {{ source('raw', 'events') }}
