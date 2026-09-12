select
    s.id,
    row_number() over (
        {{ partition_by_source_relation('demo', 'no', 's', false) }}
        order by s.updated_at desc
    ) as rn
from demo_src s
