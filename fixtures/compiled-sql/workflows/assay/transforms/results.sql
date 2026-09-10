with raw as (
    select * from external.local_raw
)
select *
from raw
join assay.raw r on r.sample_id = raw.sample_id
