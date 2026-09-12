select o.customer_id, sum(o.amount) as total
from order_filters o
join order_filters o2 on o2.customer_id = o.customer_id
group by o.customer_id
