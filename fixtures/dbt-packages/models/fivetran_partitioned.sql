select
    s.id,
    row_number() over (
        partition by s.id
        {{ fivetran_utils.partition_by_source_relation('demo', alias='s') }}
        order by s.updated_at desc
    ) as rn
from demo_src s
