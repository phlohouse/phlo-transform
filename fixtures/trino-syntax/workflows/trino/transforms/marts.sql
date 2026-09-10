with ranked as (
    select
        custkey,
        orderkey,
        item,
        row_number() over (partition by custkey order by orderkey desc) as rn
    from trino.items
)
select
    custkey,
    item,
    count(*) as item_count,
    approx_percentile(orderkey, 0.5) as median_orderkey
from ranked
where rn <= 100
group by grouping sets ((custkey, item), (item))
qualify row_number() over (partition by item order by count(*) desc) <= 10
