-- @ephemeral
select order_id, customer_id, amount from stg_orders where amount > 0
