select
    id
    {{ fivetran_utils.fill_pass_through_columns('missing_var') }}
    {{ fivetran_utils.fill_pass_through_columns('not_a_list_var') }}
from demo_src
