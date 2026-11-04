//! In-database tests, run by `cargo pgrx test`. pgrx calls every test in
//! the `tests` schema, so each file has its own `#[pg_schema] mod tests`.

use pgrx::prelude::*;

mod build;
mod compat;

pub fn file(path: &str, source: &str) {
    Spi::run_with_args(
        "INSERT INTO dbtindb_files (path, source) VALUES ($1, $2)
         ON CONFLICT (path) DO UPDATE SET source = excluded.source",
        &[path.into(), source.into()],
    )
    .unwrap();
}

/// A small shop: a source, a seed, staging view, an ephemeral model, a
/// mart table in a custom schema, tests and a snapshot.
pub fn shop() {
    Spi::run(
        "CREATE TABLE raw_orders (id int, customer_id int, country text, amount numeric, updated_at timestamp);
         INSERT INTO raw_orders VALUES
           (1, 1, 'SE', 10, '2024-01-01'), (2, 1, 'SE', 5, '2024-01-01'), (3, 2, 'NO', 7, '2024-01-01');",
    )
    .unwrap();
    file(
        "dbt_project.yml",
        "name: shop\nmodels:\n  shop:\n    marts:\n      +schema: marts\n      +materialized: table\n",
    );
    file(
        "macros/cents.sql",
        "{% macro cents(col) %}({{ col }} * 100)::int{% endmacro %}",
    );
    file(
        "seeds/countries.csv",
        "code,name,eu\nSE,Sweden,true\nNO,Norway,false\n",
    );
    file(
        "models/staging/stg_orders.sql",
        "select id, customer_id, country, {{ cents('amount') }} as amount_cents, updated_at\n\
         from {{ source('raw', 'raw_orders') }}",
    );
    file(
        "models/staging/paid_orders.sql",
        "{{ config(materialized='ephemeral') }}\n\
         select * from {{ ref('stg_orders') }} where amount_cents > {{ var('min_cents', 0) }}",
    );
    file(
        "models/marts/customers.sql",
        "select o.customer_id, sum(o.amount_cents) as total_cents, max(c.name) as country_name\n\
         from {{ ref('paid_orders') }} o left join {{ ref('countries') }} c on c.code = o.country\n\
         group by 1",
    );
    file(
        "models/schema.yml",
        "sources:\n\
         \x20 - name: raw\n\
         \x20   schema: public\n\
         \x20   tables: [{name: raw_orders}]\n\
         models:\n\
         \x20 - name: customers\n\
         \x20   description: One row per customer.\n\
         \x20   config: {persist_docs: {relation: true, columns: true}}\n\
         \x20   columns:\n\
         \x20     - name: customer_id\n\
         \x20       description: The customer.\n\
         \x20       data_tests: [unique, not_null]\n\
         \x20     - name: country_name\n\
         \x20       data_tests:\n\
         \x20         - accepted_values: {values: [Sweden, Norway], config: {severity: warn}}\n",
    );
    file(
        "snapshots/orders_snapshot.sql",
        "{% snapshot orders_snapshot %}\n\
         {{ config(unique_key='id', strategy='timestamp', updated_at='updated_at') }}\n\
         select id, amount, updated_at from {{ source('raw', 'raw_orders') }}\n\
         {% endsnapshot %}",
    );
}

pub fn count(sql: &str) -> i64 {
    Spi::get_one::<i64>(sql).unwrap().unwrap()
}

pub fn text(sql: &str) -> String {
    Spi::get_one::<String>(sql).unwrap().unwrap()
}

/// Runs `sql` in a subtransaction and returns its error message.
pub fn error_of(sql: &str) -> String {
    Spi::run(
        "CREATE OR REPLACE FUNCTION pg_temp.err(q text) RETURNS text LANGUAGE plpgsql AS $$
         BEGIN EXECUTE q; RETURN NULL; EXCEPTION WHEN OTHERS THEN RETURN SQLERRM; END $$",
    )
    .unwrap();
    Spi::get_one_with_args::<String>("SELECT pg_temp.err($1)", &[sql.into()])
        .unwrap()
        .unwrap_or_else(|| panic!("{sql} did not fail"))
}
