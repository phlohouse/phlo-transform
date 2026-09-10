select
    id as event_id,
    status,
    {{ dbt.date_trunc('day', 'ts') }} as day
from {{ ref('raw_events') }}
