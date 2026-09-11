select
    {{ dbt_utils.surrogate_key('a', 'b') }} as sk,
    {{ type_numeric() }} as num_type
from raw_events
