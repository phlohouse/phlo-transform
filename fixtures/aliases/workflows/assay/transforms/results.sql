select r.sample_id
from external.results r
join external.samples s using (sample_id)
