{% snapshot orders_snapshot %}
{{ config(strategy = 'timestamp', unique_key = 'id', updated_at = 'updated_at') }}
select * from {{ source('raw', 'orders') }}
{% endsnapshot %}
