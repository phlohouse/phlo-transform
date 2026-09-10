{{ config(materialized = 'table') }}

select
    occurred_at::date as day,
    kind,
    count(*) as n
from {{ ref('stg_events') }}
group by 1, 2
