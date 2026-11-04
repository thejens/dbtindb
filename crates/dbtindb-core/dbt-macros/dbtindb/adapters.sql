{#
    dbt's adapter macros that dbtindb implements natively or differently,
    for every dialect.
#}

{# dbt-core's macro form of adapter.get_columns_in_relation(). #}
{% macro get_columns_in_relation(relation) -%}
    {{ return(adapter.get_columns_in_relation(relation)) }}
{%- endmacro %}

{% macro get_empty_subquery_sql(select_sql, select_sql_header=none) -%}
    {{ return(adapter.dispatch('get_empty_subquery_sql', 'dbt')(select_sql, select_sql_header)) }}
{%- endmacro %}

{% macro default__get_empty_subquery_sql(select_sql, select_sql_header=none) -%}
    {%- if select_sql_header is not none -%}
    {{ select_sql_header }}
    {%- endif -%}
    select * from (
        {{ select_sql }}
    ) as __dbt_sbq
    where false
    limit 0
{%- endmacro %}

{# The names of the columns `select_sql` returns. #}
{% macro get_columns_in_query(select_sql) -%}
    {{ return(adapter.dispatch('get_columns_in_query', 'dbt')(select_sql)) }}
{%- endmacro %}

{% macro default__get_columns_in_query(select_sql) -%}
    {% call statement('get_columns_in_query', fetch_result=True, auto_begin=False) -%}
        {{ get_empty_subquery_sql(select_sql) }}
    {%- endcall %}
    {{ return(load_result('get_columns_in_query').table.columns | map(attribute='name') | list) }}
{%- endmacro %}
