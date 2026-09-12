select
    s.id,
    row_number() over (
        {{ fivetran_utils.partition_by_source_relation('demo', 'no', 's') }}
        order by s.updated_at desc
    ) as rn
from demo_src s
