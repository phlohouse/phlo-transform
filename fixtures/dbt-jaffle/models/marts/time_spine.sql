with base as (
    {{ dbt_date.get_base_dates(n_dateparts=365*2, datepart="day") }}
)
select * from base
