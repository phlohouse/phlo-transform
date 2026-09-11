select
    {{ dbt_utils.surrogate_key('a', 'b') }} as sk,
    {{ type_numeric() }} as num_type,
    {{ none | default('x') }} as with_default,
    {{ "a'b" | escape }} as escaped,
    {{ 'abc' | list }} as char_list
from raw_events
