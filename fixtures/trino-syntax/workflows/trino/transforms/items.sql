select
    o.custkey,
    o.orderkey,
    i.item
from trino.raw o
cross join unnest(array['bolt', 'nut']) as i (item)
