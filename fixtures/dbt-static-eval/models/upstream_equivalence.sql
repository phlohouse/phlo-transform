select
    {{ dbt.hash('id') }} as id_hash,
    {{ dbt.split_part('code', '-', 2) }} as code_part,
    {{ flag_or_default(true) }} as flag_value
from raw_events
