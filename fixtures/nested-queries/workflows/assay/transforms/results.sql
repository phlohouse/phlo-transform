select *
from (select * from external.inner_results) t
where sample_id in (select sample_id from external.lookup)
