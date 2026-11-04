//! dbt's built-in macros that projects rely on, written in Jinja so a
//! project can override them the way dbt allows: by defining a macro of
//! the same name (e.g. `generate_schema_name`, or `test_not_null`).

pub const BUILTINS_PATH: &str = "<dbtindb builtins>";

pub const BUILTINS: &str = r#"
{% macro default__generate_schema_name(custom_schema_name, node) -%}
    {%- set default_schema = target.schema -%}
    {%- if custom_schema_name is none -%}
        {{ default_schema }}
    {%- else -%}
        {{ default_schema }}_{{ custom_schema_name | trim }}
    {%- endif -%}
{%- endmacro %}

{% macro generate_schema_name(custom_schema_name, node) -%}
    {{ default__generate_schema_name(custom_schema_name, node) }}
{%- endmacro %}

{% macro default__generate_alias_name(custom_alias_name, node) -%}
    {%- if custom_alias_name -%}
        {{ custom_alias_name | trim }}
    {%- else -%}
        {{ node.name }}
    {%- endif -%}
{%- endmacro %}

{% macro generate_alias_name(custom_alias_name, node) -%}
    {{ default__generate_alias_name(custom_alias_name, node) }}
{%- endmacro %}

{% test not_null(model, column_name) %}
select {{ column_name }}
from {{ model }}
where {{ column_name }} is null
{% endtest %}

{% test unique(model, column_name) %}
select
    {{ column_name }} as unique_field,
    count(*) as n_records
from {{ model }}
where {{ column_name }} is not null
group by {{ column_name }}
having count(*) > 1
{% endtest %}

{% test accepted_values(model, column_name, values, quote=true) %}
with all_values as (
    select {{ column_name }} as value_field, count(*) as n_records
    from {{ model }}
    group by {{ column_name }}
)
select *
from all_values
where value_field not in (
    {%- for value in values -%}
        {%- if quote -%}'{{ value | string | replace("'", "''") }}'{%- else -%}{{ value }}{%- endif -%}
        {{- ", " if not loop.last -}}
    {%- endfor -%}
)
{% endtest %}

{% test relationships(model, column_name, to, field) %}
with child as (
    select {{ column_name }} as from_field
    from {{ model }}
    where {{ column_name }} is not null
),
parent as (
    select {{ field }} as to_field
    from {{ to }}
)
select from_field
from child
left join parent on child.from_field = parent.to_field
where parent.to_field is null
{% endtest %}
"#;
