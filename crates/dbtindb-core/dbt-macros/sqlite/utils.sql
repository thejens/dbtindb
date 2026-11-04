{#
    SQLite implementations of dbt's cross-database macros, written for
    dbtindb. SQLite has no date types: dates are 'YYYY-MM-DD' text and
    timestamps 'YYYY-MM-DD HH:MM:SS' text, so date macros keep that form
    (a date stays a date) instead of casting, and `cast(... as date)` must
    be avoided: it gives the column NUMERIC affinity and turns
    '2024-01-31' into 2024.
#}

{# The date or timestamp `d` moved by `modifier`, keeping its form. #}
{% macro sqlite__dbtindb_shift(d, modifier) -%}
    case when length({{ d }}) = 10 then date({{ d }}, {{ modifier }}) else datetime({{ d }}, {{ modifier }}) end
{%- endmacro %}

{# `d` moved by `months` months. SQLite overflows into the next month
   ('2024-01-31' + 1 month is '2024-03-02'); PostgreSQL stops at the last
   day of the month ('2024-02-29'), and so does this: it takes whichever is
   earlier, the overflowed date or the target month's last day. #}
{% macro sqlite__dbtindb_add_months(d, months) -%}
    {%- set last_day = "date(" ~ d ~ ", 'start of month', ((" ~ months ~ ") + 1) || ' months', '-1 day')" -%}
    case when length({{ d }}) = 10
        then min(date({{ d }}, ({{ months }}) || ' months'), {{ last_day }})
        else min(datetime({{ d }}, ({{ months }}) || ' months'), {{ last_day }} || ' ' || time({{ d }}))
    end
{%- endmacro %}

{% macro sqlite__dateadd(datepart, interval, from_date_or_timestamp) -%}
    {%- set part = datepart | lower -%}
    {%- set days = {'day': '1', 'week': '7'} -%}
    {%- set months = {'month': '1', 'quarter': '3', 'year': '12'} -%}
    {%- set times = {'hour': 'hours', 'minute': 'minutes', 'second': 'seconds'} -%}
    {%- if part in days -%}
        {{ sqlite__dbtindb_shift(from_date_or_timestamp, "((" ~ interval ~ ") * " ~ days[part] ~ ") || ' days'") }}
    {%- elif part in months -%}
        {{ sqlite__dbtindb_add_months(from_date_or_timestamp, "(" ~ interval ~ ") * " ~ months[part]) }}
    {%- elif part in times -%}
        datetime({{ from_date_or_timestamp }}, ({{ interval }}) || ' {{ times[part] }}')
    {%- else -%}
        {{ exceptions.raise_compiler_error("dateadd: datepart " ~ datepart ~ " is not supported on SQLite") }}
    {%- endif -%}
{%- endmacro %}

{% macro sqlite__dbtindb_part(format, d) -%}
    cast(strftime('{{ format }}', {{ d }}) as integer)
{%- endmacro %}

{% macro sqlite__datediff(first_date, second_date, datepart) -%}
    {#- Counts boundaries crossed, as dbt-postgres does: datediff('2024-01-31',
        '2024-02-01', 'month') is 1. -#}
    {%- set part = datepart | lower -%}
    {%- set a, b = first_date, second_date -%}
    {%- if part == 'year' -%}
        ({{ sqlite__dbtindb_part('%Y', b) }} - {{ sqlite__dbtindb_part('%Y', a) }})
    {%- elif part == 'quarter' -%}
        ({{ sqlite__datediff(a, b, 'year') }} * 4 + ({{ sqlite__dbtindb_part('%m', b) }} - 1) / 3 - ({{ sqlite__dbtindb_part('%m', a) }} - 1) / 3)
    {%- elif part == 'month' -%}
        ({{ sqlite__datediff(a, b, 'year') }} * 12 + {{ sqlite__dbtindb_part('%m', b) }} - {{ sqlite__dbtindb_part('%m', a) }})
    {%- elif part == 'day' -%}
        cast(julianday(date({{ b }})) - julianday(date({{ a }})) as integer)
    {%- elif part == 'week' -%}
        ({{ sqlite__datediff(a, b, 'day') }} / 7 + case
            when {{ sqlite__dbtindb_part('%w', a) }} <= {{ sqlite__dbtindb_part('%w', b) }} then
                case when {{ a }} <= {{ b }} then 0 else -1 end
            else
                case when {{ a }} <= {{ b }} then 1 else 0 end
        end)
    {%- elif part == 'hour' -%}
        ({{ sqlite__datediff(a, b, 'day') }} * 24 + {{ sqlite__dbtindb_part('%H', b) }} - {{ sqlite__dbtindb_part('%H', a) }})
    {%- elif part == 'minute' -%}
        ({{ sqlite__datediff(a, b, 'hour') }} * 60 + {{ sqlite__dbtindb_part('%M', b) }} - {{ sqlite__dbtindb_part('%M', a) }})
    {%- elif part == 'second' -%}
        ({{ sqlite__datediff(a, b, 'minute') }} * 60 + {{ sqlite__dbtindb_part('%S', b) }} - {{ sqlite__dbtindb_part('%S', a) }})
    {%- else -%}
        {{ exceptions.raise_compiler_error("datediff: datepart " ~ datepart ~ " is not supported on SQLite") }}
    {%- endif -%}
{%- endmacro %}

{% macro sqlite__date_trunc(datepart, date) -%}
    {%- set part = datepart | lower -%}
    {%- if part == 'day' -%}
        date({{ date }})
    {%- elif part == 'week' -%}
        {#- Weeks start on Monday, as in PostgreSQL. -#}
        date({{ date }}, '-' || ((cast(strftime('%w', {{ date }}) as integer) + 6) % 7) || ' days')
    {%- elif part == 'month' -%}
        date({{ date }}, 'start of month')
    {%- elif part == 'quarter' -%}
        date({{ date }}, 'start of month', '-' || ((cast(strftime('%m', {{ date }}) as integer) - 1) % 3) || ' months')
    {%- elif part == 'year' -%}
        date({{ date }}, 'start of year')
    {%- elif part == 'hour' -%}
        strftime('%Y-%m-%d %H:00:00', {{ date }})
    {%- elif part == 'minute' -%}
        strftime('%Y-%m-%d %H:%M:00', {{ date }})
    {%- elif part == 'second' -%}
        strftime('%Y-%m-%d %H:%M:%S', {{ date }})
    {%- else -%}
        {{ exceptions.raise_compiler_error("date_trunc: datepart " ~ datepart ~ " is not supported on SQLite") }}
    {%- endif -%}
{%- endmacro %}

{% macro sqlite__last_day(date, datepart) -%}
    {%- set part = datepart | lower -%}
    {%- set lengths = {'week': "'+6 days'", 'month': "'+1 month', '-1 day'",
                       'quarter': "'+3 months', '-1 day'", 'year': "'+1 year', '-1 day'"} -%}
    {%- if part in lengths -%}
        date({{ sqlite__date_trunc(part, date) }}, {{ lengths[part] }})
    {%- else -%}
        {{ exceptions.raise_compiler_error("last_day: datepart " ~ datepart ~ " is not supported on SQLite") }}
    {%- endif -%}
{%- endmacro %}

{% macro sqlite__date(year, month, day) -%}
    '{{ modules.datetime.date(year, month, day).strftime('%Y-%m-%d') }}'
{%- endmacro %}

{% macro sqlite__current_timestamp() -%}
    strftime('%Y-%m-%d %H:%M:%f', 'now')
{%- endmacro %}

{% macro sqlite__current_timestamp_backcompat() -%}
    {{ sqlite__current_timestamp() }}
{%- endmacro %}

{% macro sqlite__current_timestamp_in_utc_backcompat() -%}
    {{ sqlite__current_timestamp() }}
{%- endmacro %}

{% macro sqlite__snapshot_string_as_time(timestamp) -%}
    '{{ timestamp }}'
{%- endmacro %}

{# The sqlite_dbtindb extension registers dbtindb_split_part, which
   behaves as PostgreSQL's split_part. #}
{% macro sqlite__split_part(string_text, delimiter_text, part_number) -%}
    dbtindb_split_part({{ string_text }}, {{ delimiter_text }}, {{ part_number }})
{%- endmacro %}

{% macro sqlite__listagg(measure, delimiter_text, order_by_clause, limit_num) -%}
    {%- if limit_num -%}
        {{ exceptions.raise_compiler_error("listagg: limit_num is not supported on SQLite") }}
    {%- endif -%}
    {#- An ORDER BY inside an aggregate needs SQLite 3.44 or newer. -#}
    group_concat({{ measure }}, {{ delimiter_text }}{% if order_by_clause %} {{ order_by_clause }}{% endif %})
{%- endmacro %}

{% macro sqlite__any_value(expression) -%}
    min({{ expression }})
{%- endmacro %}

{% macro sqlite__bool_or(expression) -%}
    max({{ expression }})
{%- endmacro %}

{% macro sqlite__position(substring_text, string_text) -%}
    instr({{ string_text }}, {{ substring_text }})
{%- endmacro %}

{% macro sqlite__right(string_text, length_expression) -%}
    case when ({{ length_expression }}) = 0 then '' else substr({{ string_text }}, -({{ length_expression }})) end
{%- endmacro %}

{% macro sqlite__hash(field) -%}
    dbtindb_md5(cast({{ field }} as text))
{%- endmacro %}

{% macro sqlite__cast_bool_to_text(field) -%}
    case when {{ field }} is null then null when {{ field }} then 'true' else 'false' end
{%- endmacro %}

{# SQLite has recursive CTEs but not necessarily power(). #}
{% macro sqlite__generate_series(upper_bound) -%}
    with recursive dbtindb_series(generated_number) as (
        select 1
        union all
        select generated_number + 1 from dbtindb_series where generated_number < {{ upper_bound }}
    )
    select generated_number from dbtindb_series
{%- endmacro %}

{% macro sqlite__dbtindb_no_arrays() -%}
    {{ exceptions.raise_compiler_error("SQLite has no array type") }}
{%- endmacro %}

{% macro sqlite__array_construct(inputs, data_type) -%}{{ sqlite__dbtindb_no_arrays() }}{%- endmacro %}
{% macro sqlite__array_append(array, new_element) -%}{{ sqlite__dbtindb_no_arrays() }}{%- endmacro %}
{% macro sqlite__array_concat(array_1, array_2) -%}{{ sqlite__dbtindb_no_arrays() }}{%- endmacro %}
