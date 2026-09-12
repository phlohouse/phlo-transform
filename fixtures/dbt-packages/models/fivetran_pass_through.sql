select
    id
    {{ fivetran_utils.fill_pass_through_columns('demo_pass_through') }}
from demo_src
