{{ config(materialized = 'ephemeral') }}

select customer_id, count(*) as n from {{ ref('stg_orders') }} group by customer_id
