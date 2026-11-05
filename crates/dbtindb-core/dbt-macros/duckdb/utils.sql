{#-
  dbtindb's DuckDB versions of dbt's cross-database macros, for the ones
  whose default__ version DuckDB cannot run. DuckDB runs the defaults of
  the rest (any_value, split_part with negative parts, position, right,
  hash, arrays, ...) natively.
-#}

{% macro duckdb__dateadd(datepart, interval, from_date_or_timestamp) -%}
    ({{ from_date_or_timestamp }} + ((interval '1 {{ datepart }}') * ({{ interval }})))
{%- endmacro %}

{#- DuckDB's date_diff counts boundaries crossed, which is dbt's definition. -#}
{% macro duckdb__datediff(first_date, second_date, datepart) -%}
    date_diff('{{ datepart }}', ({{ first_date }})::timestamp, ({{ second_date }})::timestamp)
{%- endmacro %}

{% macro duckdb__listagg(measure, delimiter_text, order_by_clause, limit_num) -%}
    {% if limit_num -%}
    array_to_string(
        list_slice(list({{ measure }} {{ order_by_clause or '' }}), 1, {{ limit_num }}),
        {{ delimiter_text }}
    )
    {%- else -%}
    string_agg({{ measure }}, {{ delimiter_text }} {{ order_by_clause or '' }})
    {%- endif %}
{%- endmacro %}

{% macro duckdb__date(year, month, day) -%}
    make_date({{ year }}, {{ month }}, {{ day }})
{%- endmacro %}

{% macro duckdb__current_timestamp() -%}
    now()
{%- endmacro %}

{% macro duckdb__current_timestamp_backcompat() -%}
    current_timestamp::{{ type_timestamp() }}
{%- endmacro %}

{% macro duckdb__current_timestamp_in_utc_backcompat() -%}
    (current_timestamp at time zone 'utc')::{{ type_timestamp() }}
{%- endmacro %}

{% macro duckdb__snapshot_string_as_time(timestamp) -%}
    {{ return("'" ~ timestamp ~ "'::timestamp") }}
{%- endmacro %}

{% macro duckdb__snapshot_get_time() -%}
    now()::timestamp
{%- endmacro %}
