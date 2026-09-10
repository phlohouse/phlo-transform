select
    {{ dbt_utils.star(from = ref('stg_customers'), except = ['region']) }}
from {{ ref('stg_customers') }}
