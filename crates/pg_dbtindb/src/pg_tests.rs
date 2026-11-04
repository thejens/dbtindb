use pgrx::prelude::*;

#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    fn file(path: &str, source: &str) {
        Spi::run_with_args(
            "INSERT INTO dbtindb_files (path, source) VALUES ($1, $2)
             ON CONFLICT (path) DO UPDATE SET source = excluded.source",
            &[path.into(), source.into()],
        )
        .unwrap();
    }

    /// A small shop: a source, a seed, staging view, an ephemeral model, a
    /// mart table in a custom schema, tests and a snapshot.
    fn shop() {
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

    fn count(sql: &str) -> i64 {
        Spi::get_one::<i64>(sql).unwrap().unwrap()
    }

    fn text(sql: &str) -> String {
        Spi::get_one::<String>(sql).unwrap().unwrap()
    }

    /// Runs `sql` in a subtransaction and returns its error message.
    fn error_of(sql: &str) -> String {
        Spi::run(
            "CREATE OR REPLACE FUNCTION pg_temp.err(q text) RETURNS text LANGUAGE plpgsql AS $$
             BEGIN EXECUTE q; RETURN NULL; EXCEPTION WHEN OTHERS THEN RETURN SQLERRM; END $$",
        )
        .unwrap();
        Spi::get_one_with_args::<String>("SELECT pg_temp.err($1)", &[sql.into()])
            .unwrap()
            .unwrap_or_else(|| panic!("{sql} did not fail"))
    }

    #[pg_test]
    fn builds_the_project_in_dag_order() {
        shop();
        let steps = text(
            "SELECT string_agg(name || ':' || action || ':' || status, ', ' ORDER BY step)
               FROM dbtindb_build(target_schema => 'analytics')",
        );
        assert_eq!(
            steps,
            "stg_orders:create view:success, countries:create seed:success, \
             customers:create table:success, orders_snapshot:create snapshot:success, \
             accepted_values_customers_country_name:test:pass, \
             not_null_customers_customer_id:test:pass, unique_customers_customer_id:test:pass"
        );
        // The custom schema is <target>_<custom>, as dbt's default
        // generate_schema_name makes it, and is created on demand.
        assert_eq!(
            count("SELECT total_cents FROM analytics_marts.customers WHERE customer_id = 1"),
            1500
        );
        assert_eq!(
            text("SELECT country_name FROM analytics_marts.customers WHERE customer_id = 2"),
            "Norway"
        );
        assert_eq!(text("SELECT data_type FROM information_schema.columns WHERE table_name = 'countries' AND column_name = 'eu'"), "boolean");
        assert_eq!(
            count("SELECT count(*) FROM analytics.orders_snapshot WHERE dbt_valid_to IS NULL"),
            3
        );
        assert_eq!(
            text("SELECT obj_description('analytics_marts.customers'::regclass)"),
            "One row per customer."
        );
        assert_eq!(
            text("SELECT col_description('analytics_marts.customers'::regclass, 1)"),
            "The customer."
        );
        assert_eq!(
            count("SELECT count(*) FROM pg_class WHERE relname = 'paid_orders'"),
            0
        );
        assert_eq!(count("SELECT count(*) FROM dbtindb_managed"), 4);
    }

    #[pg_test]
    fn lists_and_compiles() {
        shop();
        assert_eq!(
            text(
                "SELECT string_agg(name || ':' || coalesce(materialized, '-') || ':' || coalesce(relation, '-'), ', ' ORDER BY build_order)
                   FROM dbtindb_ls('+customers', exclude => 'resource_type:test', target_schema => 'analytics')"
            ),
            r#"stg_orders:view:"analytics"."stg_orders", paid_orders:ephemeral:-, countries:seed:"analytics"."countries", customers:table:"analytics_marts"."customers""#
        );
        assert_eq!(
            text("SELECT depends_on::text FROM dbtindb_ls('customers') WHERE name = 'customers'"),
            "{model.shop.paid_orders,seed.shop.countries}"
        );
        let sql = text(r#"SELECT dbtindb_compile('customers', '{"min_cents": 600}')"#);
        assert!(
            sql.starts_with(r#"with "__dbt__cte__paid_orders" as ("#),
            "{sql}"
        );
        assert!(sql.contains("amount_cents > 600"), "{sql}");
    }

    #[pg_test]
    fn rebuilds_dependent_views_and_reloads_seeds() {
        shop();
        Spi::run("SELECT * FROM dbtindb_build(target_schema => 'analytics')").unwrap();
        let steps = text(
            "SELECT string_agg(name || ':' || action, ', ' ORDER BY step)
               FROM dbtindb_run('stg_orders', target_schema => 'analytics')",
        );
        // customers is a table, so only the view itself is rebuilt.
        assert_eq!(steps, "stg_orders:create view");
        file(
            "models/marts/report.sql",
            "{{ config(materialized='view') }} select * from {{ ref('customers') }}",
        );
        let steps = text(
            "SELECT string_agg(name || ':' || action, ', ' ORDER BY step)
               FROM dbtindb_run('customers', target_schema => 'analytics')",
        );
        assert_eq!(steps, "customers:create table");
        let steps = text(
            "SELECT string_agg(name || ':' || action, ', ' ORDER BY step)
               FROM dbtindb_run('customers report', target_schema => 'analytics')",
        );
        assert_eq!(steps, "customers:create table, report:create view");
        // Replacing customers rebuilds the managed view on top of it.
        let steps = text(
            "SELECT string_agg(name || ':' || action, ', ' ORDER BY step)
               FROM dbtindb_run('customers', target_schema => 'analytics')",
        );
        assert_eq!(steps, "customers:create table, report:create view");
        let steps = text(
            "SELECT string_agg(name || ':' || action, ', ' ORDER BY step)
               FROM dbtindb_seed(target_schema => 'analytics')",
        );
        assert_eq!(steps, "countries:reload seed");
    }

    #[pg_test]
    fn incremental_merge_and_schema_changes() {
        Spi::run("CREATE TABLE src (id int, v text); INSERT INTO src VALUES (1, 'a'), (2, 'b')")
            .unwrap();
        file(
            "models/inc.sql",
            "{{ config(materialized='incremental', unique_key='id', incremental_strategy='merge', \
             on_schema_change='append_new_columns') }}\n\
             select * from src\n\
             {% if is_incremental() %}where id >= (select max(id) from {{ this }}){% endif %}",
        );
        let action = text("SELECT action FROM dbtindb_run()");
        assert_eq!(action, "create table");
        Spi::run("ALTER TABLE src ADD COLUMN w int; UPDATE src SET v = 'B', w = 7 WHERE id = 2; INSERT INTO src VALUES (3, 'c', 8)").unwrap();
        if count("SELECT current_setting('server_version_num')::bigint") < 150000 {
            let err = error_of("SELECT * FROM dbtindb_run()");
            assert!(err.contains("MERGE needs PostgreSQL 15 or newer"), "{err}");
            return;
        }
        let action = text("SELECT action FROM dbtindb_run()");
        assert_eq!(action, "incremental (merge)");
        assert_eq!(
            text("SELECT string_agg(id || v || coalesce(w::text, '-'), ',' ORDER BY id) FROM public.inc"),
            "1a-,2B7,3c8"
        );
        let action = text("SELECT action FROM dbtindb_run(full_refresh => true)");
        assert_eq!(action, "create table");
    }

    #[pg_test]
    fn refreshes_materialized_views_until_their_query_changes() {
        Spi::run("CREATE TABLE src (id int); INSERT INTO src VALUES (1)").unwrap();
        file(
            "models/mv.sql",
            "{{ config(materialized='materialized_view') }} select count(*) as n from src",
        );
        assert_eq!(
            text("SELECT action FROM dbtindb_run()"),
            "create materialized view"
        );
        Spi::run("INSERT INTO src VALUES (2)").unwrap();
        assert_eq!(
            text("SELECT action FROM dbtindb_run()"),
            "refresh materialized view"
        );
        assert_eq!(count("SELECT n FROM public.mv"), 2);
        file(
            "models/mv.sql",
            "{{ config(materialized='materialized_view') }} select count(*) + 1 as n from src",
        );
        assert_eq!(
            text("SELECT action FROM dbtindb_run()"),
            "create materialized view"
        );
        assert_eq!(count("SELECT n FROM public.mv"), 3);
    }

    #[pg_test]
    fn run_query_returns_typed_values() {
        Spi::run("CREATE TABLE src (id int, price numeric, ok bool, meta jsonb)").unwrap();
        Spi::run(r#"INSERT INTO src VALUES (1, 2.5, true, '{"a": 1}'), (2, 4, false, null)"#)
            .unwrap();
        file(
            "models/m.sql",
            "{% set t = run_query('select id, price, ok, meta from src order by id') %}\n\
             {% if execute %}{% for r in t.rows %}\
             select {{ r[0] + 1 }} as id, {{ r[1] * 2 }} as price, {{ 'true' if r[2] else 'false' }} as ok, \
             '{{ r[3] | tojson if r[3] is not none else 'none' }}' as meta\
             {% if not loop.last %} union all {% endif %}{% endfor %}\
             {% else %}select 1{% endif %}",
        );
        Spi::run("SELECT * FROM dbtindb_run()").unwrap();
        assert_eq!(
            text("SELECT string_agg(id || '/' || price || '/' || ok || '/' || meta, ', ' ORDER BY id) FROM public.m"),
            r#"2/5.0/true/{"a": 1}, 3/8/false/none"#
        );
    }

    #[pg_test]
    fn packages_statements_and_cross_database_macros() {
        file("dbt_packages/tools/dbt_project.yml", "name: tools\n");
        file(
            "dbt_packages/tools/macros/tools.sql",
            "{% macro answer() %}{% call statement('a', fetch_result=True) %}select 41 + 1 as n{% endcall %}\
             {{ return(load_result('a').data[0][0]) }}{% endmacro %}\n\
             {% macro column_list(sql) %}{{ return(get_columns_in_query(sql) | join(',')) }}{% endmacro %}\n\
             {% test sums_to(model, column_name, total) %}{{ config(fail_calc='sum(diff)') }}\
             select abs(sum({{ column_name }}) - {{ total }}) as diff from {{ model }}{% endtest %}",
        );
        file(
            "models/m.sql",
            "select {{ tools.answer() if execute else 0 }} as answer,\n\
             '{{ tools.column_list('select 1 as a, 2 as b') if execute }}' as cols,\n\
             ({{ dbt.dateadd('month', 1, \"'2024-01-31'::date\") }})::date as next_month,\n\
             {{ dbt.datediff(\"'2024-01-31'::date\", \"'2024-03-01'::date\", 'month') }} as months,\n\
             {{ dbt.split_part(\"'a-b-c'\", \"'-'\", 2) }} as part,\n\
             {{ dbt.hash(\"'x'\") }} as h",
        );
        file(
            "models/m.yml",
            "models:\n  - name: m\n    columns:\n      - name: answer\n        data_tests: [{tools.sums_to: {total: 42}}]\n",
        );
        assert_eq!(
            count("SELECT count(*) FROM dbtindb_build() WHERE status IN ('success', 'pass')"),
            2
        );
        assert_eq!(
            text("SELECT concat_ws('|', answer, cols, next_month, months, part, h) FROM public.m"),
            "42|a,b|2024-02-29|2|b|9dd4e461268c8034f5c8564e155c67a6"
        );
        file(
            "models/m.yml",
            "models:\n  - name: m\n    columns:\n      - name: answer\n        data_tests: [{tools.sums_to: {total: 40}}]\n",
        );
        assert_eq!(
            count("SELECT failures FROM dbtindb_test() WHERE name = 'tools_sums_to_m_answer'"),
            2
        );
    }

    #[pg_test]
    fn failing_tests() {
        shop();
        Spi::run("INSERT INTO raw_orders VALUES (4, 3, 'DK', 1, '2024-01-01')").unwrap();
        file(
            "seeds/countries.csv",
            "code,name,eu\nSE,Sweden,true\nNO,Norway,false\nDK,Denmark,true\n",
        );
        let warn = text(
            "SELECT status || ':' || failures FROM dbtindb_build(target_schema => 'analytics')
              WHERE name = 'accepted_values_customers_country_name'",
        );
        assert_eq!(warn, "warn:1");
        // An error-severity failure aborts a build, and with it everything
        // the build did.
        Spi::run("INSERT INTO raw_orders VALUES (5, NULL, 'SE', 1, '2024-01-01')").unwrap();
        let err = error_of("SELECT * FROM dbtindb_build(target_schema => 'analytics')");
        assert!(
            err.contains("test not_null_customers_customer_id failed with 1 failing rows"),
            "{err}"
        );
        assert_eq!(count("SELECT count(*) FROM analytics_marts.customers"), 3);
        // dbtindb_test reports instead.
        assert_eq!(
            text("SELECT status FROM dbtindb_test('customers', target_schema => 'analytics') WHERE name = 'not_null_customers_customer_id'"),
            "pass",
            "customers was not rebuilt, so it still has no NULL customer"
        );
    }

    #[pg_test]
    fn never_replaces_relations_it_did_not_create() {
        shop();
        Spi::run(
            "CREATE SCHEMA analytics_marts; CREATE TABLE analytics_marts.customers (precious int)",
        )
        .unwrap();
        let err = error_of("SELECT * FROM dbtindb_build(target_schema => 'analytics')");
        assert!(err.contains("was not created by dbtindb"), "{err}");
        assert_eq!(count("SELECT count(*) FROM information_schema.columns WHERE table_name = 'customers' AND column_name = 'precious'"), 1);
    }

    #[pg_test]
    fn a_failing_model_changes_nothing() {
        shop();
        Spi::run("SELECT * FROM dbtindb_build(target_schema => 'analytics')").unwrap();
        file(
            "models/broken.sql",
            "select * from {{ ref('customers') }} where nope",
        );
        let err = error_of(
            r#"SELECT * FROM dbtindb_run(vars => '{"min_cents": 600}', target_schema => 'analytics')"#,
        );
        assert!(err.contains("nope"), "{err}");
        assert_eq!(
            count("SELECT total_cents FROM analytics_marts.customers WHERE customer_id = 1"),
            1500
        );
        assert_eq!(count("SELECT count(*) FROM dbtindb_managed"), 4);
    }

    #[pg_test]
    fn dependents_that_are_not_models_block_the_rebuild() {
        shop();
        Spi::run("SELECT * FROM dbtindb_build(target_schema => 'analytics')").unwrap();
        Spi::run("CREATE VIEW public.report AS SELECT * FROM analytics_marts.customers").unwrap();
        let err = error_of("SELECT * FROM dbtindb_run(target_schema => 'analytics')");
        assert!(err.contains("depend"), "{err}");
        assert_eq!(count("SELECT count(*) FROM public.report"), 2);
    }

    #[pg_test]
    fn renders_and_views_inline_sql() {
        shop();
        Spi::run("SELECT * FROM dbtindb_build(target_schema => 'analytics')").unwrap();
        let sql = text(
            "SELECT dbtindb_sql($$select count(*) as n from {{ ref('customers') }} where total_cents > {{ var('min', 0) }}$$, \
             '{\"min\": 1000}', 'analytics')",
        );
        assert_eq!(
            sql,
            r#"select count(*) as n from "analytics_marts"."customers" where total_cents > 1000"#
        );
        let sql = text(
            "SELECT dbtindb_view('big', $$select customer_id from {{ ref('customers') }} where total_cents > 1000$$, \
             target_schema => 'analytics')",
        );
        assert!(sql.contains(r#""analytics_marts"."customers""#), "{sql}");
        assert_eq!(count("SELECT customer_id::bigint FROM big"), 1);
        // Replacing it may change its columns.
        Spi::run("SELECT dbtindb_view('big', 'select 1 as a, 2 as b')").unwrap();
        assert_eq!(count("SELECT b::bigint FROM big"), 2);
        assert_eq!(count("SELECT count(*) FROM pg_views WHERE viewname = 'big' AND schemaname LIKE 'pg_temp%'"), 1);
    }

    #[pg_test]
    fn cannot_run_from_inside_a_build() {
        file(
            "models/m.sql",
            "{{ config(materialized='table') }} select count(*) from dbtindb_run()",
        );
        let err = error_of("SELECT * FROM dbtindb_run()");
        assert!(err.contains("cannot run while another is running"), "{err}");
    }

    #[pg_test]
    fn reports_project_errors() {
        file("models/a.sql", "select * from {{ ref('b') }}");
        file("models/b.sql", "select * from {{ ref('a') }}");
        let err = error_of("SELECT * FROM dbtindb_ls()");
        assert!(err.contains("dependency cycle: a -> b -> a"), "{err}");

        let err = error_of("SELECT dbtindb_compile('missing')");
        assert!(
            err.contains("no enabled model, snapshot or test named \"missing\""),
            "{err}"
        );
    }

    #[pg_test]
    fn rejects_identifiers_postgres_would_truncate() {
        let long = "x".repeat(64);
        file(&format!("models/{long}.sql"), "select 1");
        let err = error_of("SELECT * FROM dbtindb_ls()");
        assert!(err.contains("63-byte identifier limit"), "{err}");
    }
}
