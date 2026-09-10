with raw as (
    select * from external.local_raw
)
select * from raw
