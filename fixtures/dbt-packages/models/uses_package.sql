select
  {{ kit.squared('price') }} as price_sq,
  {{ localpkg.prefixed('stg', 'orders') }} as prefixed,
  {{ kit.unroll(['a', 'b']) }} as unrolled,
  {{ signature() }} as sig
from raw_orders
