select
    r.sample_id,
    d.date_key
from assay.raw r
join shared.dimensions.date d on r.created_on = d.date_key
