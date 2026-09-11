with base as (
    {{ dbt_date.get_base_dates(n_dateparts=30, datepart="day") }}
)
select * from base
