select
    id,
    {% for row in run_query('select 1') %}
    1,
    {% endfor %}
    status
from {{ ref('orders') }}
