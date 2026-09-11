select {{ dbt_utils.star(from = ref('stg_customers'), rename = {'id': 'customer_id'}) }}
from {{ ref('stg_customers') }}
