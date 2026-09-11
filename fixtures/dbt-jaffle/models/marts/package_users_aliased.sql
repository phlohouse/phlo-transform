select {{ dbt_utils.star(from = ref('stg_customers'), relation_alias = 'c') }}
from {{ ref('stg_customers') }} c
