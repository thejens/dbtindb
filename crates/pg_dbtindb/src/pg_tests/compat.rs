//! dbt features beyond building: packages, grants, docs, stored test
//! failures, YAML snapshots and unit tests.

use pgrx::prelude::*;

#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    use crate::pg_tests::*;

    #[pg_test]
    fn grants_follow_the_config_and_docs_become_comments() {
        Spi::run(
            "CREATE ROLE dbtindb_reader; CREATE ROLE dbtindb_writer; CREATE ROLE dbtindb_old;
             CREATE TABLE src (id int); INSERT INTO src VALUES (1)",
        )
        .unwrap();
        file(
            "dbt_project.yml",
            "name: shop\nmodels:\n  +grants: {select: [dbtindb_reader]}\n",
        );
        file(
            "models/inc.sql",
            "{{ config(materialized='incremental', grants={'+select': ['dbtindb_writer'], 'insert': ['dbtindb_writer']}) }}\n\
             select * from src",
        );
        file(
            "models/docs.md",
            "{% docs inc %}\nOne row per *id*.\n{% enddocs %}",
        );
        file(
            "models/schema.yml",
            "models:\n  - name: inc\n    description: \"{{ doc('inc') }}\"\n    \
             config: {persist_docs: {relation: true}}\n",
        );
        let grants = || {
            text(
                "SELECT string_agg(grantee || ':' || privilege_type, ',' ORDER BY grantee, privilege_type)
                 FROM information_schema.role_table_grants
                 WHERE table_name = 'inc' AND grantee LIKE 'dbtindb_%'",
            )
        };
        Spi::run("SELECT * FROM dbtindb_run()").unwrap();
        assert_eq!(
            grants(),
            "dbtindb_reader:SELECT,dbtindb_writer:INSERT,dbtindb_writer:SELECT"
        );
        assert_eq!(
            text("SELECT obj_description('public.inc'::regclass)"),
            "One row per *id*."
        );
        // The incremental table is kept, so grants made outside the config
        // are revoked and the ones the config dropped go away.
        Spi::run("GRANT UPDATE ON public.inc TO dbtindb_old").unwrap();
        file(
            "models/inc.sql",
            "{{ config(materialized='incremental', grants={'insert': []}) }}\nselect * from src",
        );
        assert_eq!(
            text("SELECT action FROM dbtindb_run()"),
            "incremental (append)"
        );
        assert_eq!(grants(), "dbtindb_reader:SELECT");
    }

    #[pg_test]
    fn tests_store_their_failing_rows() {
        Spi::run("CREATE TABLE src (id int); INSERT INTO src VALUES (1), (1), (2), (NULL)")
            .unwrap();
        file(
            "dbt_project.yml",
            "name: shop\nmodels:\n  +materialized: table\ndata_tests:\n  +store_failures: true\n",
        );
        file("models/m.sql", "select * from src");
        file(
            "models/schema.yml",
            "models:\n  - name: m\n    columns:\n      - name: id\n        data_tests:\n          \
             - unique\n          - not_null: {config: {store_failures_as: view}}\n",
        );
        Spi::run("SELECT * FROM dbtindb_run()").unwrap();
        assert_eq!(
            text("SELECT string_agg(name || ':' || status, ',' ORDER BY name) FROM dbtindb_test()"),
            "not_null_m_id:fail,unique_m_id:fail"
        );
        assert_eq!(
            count("SELECT n_records FROM public_dbt_test__audit.unique_m_id"),
            2
        );
        assert_eq!(
            text("SELECT relkind::text FROM pg_class WHERE relname = 'not_null_m_id'"),
            "v"
        );
        // The failing-rows view would block rebuilding m; it goes, and
        // comes back with the next test run.
        Spi::run("DELETE FROM src WHERE id IS NULL").unwrap();
        Spi::run("SELECT * FROM dbtindb_run()").unwrap();
        assert_eq!(
            count("SELECT count(*) FROM pg_class WHERE relname = 'not_null_m_id'"),
            0
        );
        assert_eq!(
            text("SELECT status FROM dbtindb_test() WHERE name = 'not_null_m_id'"),
            "pass"
        );
        assert_eq!(
            count("SELECT count(*) FROM public_dbt_test__audit.not_null_m_id"),
            0
        );
    }

    #[pg_test]
    fn yaml_snapshots_record_deletes() {
        Spi::run(
            "CREATE TABLE people (id int, name text, updated_at timestamp);
             INSERT INTO people VALUES (1, 'ada', '2024-01-01'), (2, 'bob', '2024-01-01')",
        )
        .unwrap();
        file(
            "snapshots/people.yml",
            "sources:\n  - name: raw\n    schema: public\n    tables: [{name: people}]\n\
             snapshots:\n  - name: people_snap\n    relation: source('raw', 'people')\n    config:\n      \
             unique_key: id\n      strategy: timestamp\n      updated_at: updated_at\n      hard_deletes: new_record\n",
        );
        Spi::run("SELECT * FROM dbtindb_snapshot()").unwrap();
        Spi::run("DELETE FROM people WHERE id = 2").unwrap();
        Spi::run("SELECT * FROM dbtindb_snapshot()").unwrap();
        Spi::run("SELECT * FROM dbtindb_snapshot()").unwrap();
        assert_eq!(
            text(
                "SELECT string_agg(id || dbt_is_deleted || (dbt_valid_to IS NULL), ',' ORDER BY id, dbt_valid_from)
                 FROM public.people_snap"
            ),
            "1Falsetrue,2Falsefalse,2Truetrue"
        );
        assert_eq!(
            count(
                "SELECT count(*) FROM public.people_snap a JOIN public.people_snap b USING (id)
                 WHERE a.dbt_is_deleted = 'False' AND b.dbt_is_deleted = 'True' AND a.dbt_valid_to = b.dbt_valid_from"
            ),
            1
        );
    }

    #[pg_test]
    fn unit_tests_compare_the_model_on_fixture_rows() {
        Spi::run("CREATE TABLE orders (id int, customer_id int, amount numeric, ordered_at date)")
            .unwrap();
        file(
            "models/totals.sql",
            "{{ config(materialized='table') }}\n\
             select customer_id, sum(amount) as total, max(ordered_at) as last_order\n\
             from {{ source('raw', 'orders') }} where ordered_at >= '{{ var('since', '2000-01-01') }}' group by 1",
        );
        file(
            "models/schema.yml",
            "sources:\n  - name: raw\n    schema: public\n    tables: [{name: orders}]\n\
             unit_tests:\n  - name: totals_since\n    model: totals\n    \
             overrides: {vars: {since: '2024-02-01'}}\n    given:\n      \
             - input: source('raw', 'orders')\n        format: csv\n        rows: |\n          \
             id,customer_id,amount,ordered_at\n          1,1,2.5,2024-01-15\n          \
             2,1,4.25,2024-02-02\n          3,2,1,2024-03-01\n    expect:\n      rows:\n        \
             - {customer_id: 1, total: 4.25, last_order: 2024-02-02}\n        \
             - {customer_id: 2, total: 1, last_order: 2024-03-01}\n",
        );
        Spi::run("SELECT * FROM dbtindb_run()").unwrap();
        assert_eq!(
            text("SELECT status || ':' || failures FROM dbtindb_test() WHERE resource_type = 'unit_test'"),
            "pass:0"
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
}
