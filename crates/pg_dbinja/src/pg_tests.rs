use pgrx::prelude::*;

#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    fn template(name: &str, source: &str) {
        Spi::run_with_args(
            "INSERT INTO jinja_templates (name, source) VALUES ($1, $2)",
            &[name.into(), source.into()],
        )
        .unwrap();
    }

    fn jaffle_shop() {
        Spi::run("CREATE TABLE raw_orders (id int, customer_id int, amount numeric)").unwrap();
        Spi::run("INSERT INTO raw_orders VALUES (1, 1, 10), (2, 1, 5), (3, 2, 7)").unwrap();
        template(
            "macros/cents.sql",
            "{% macro cents(col) %}({{ col }} * 100)::int{% endmacro %}",
        );
        template(
            "models/staging/stg_orders.sql",
            "select id, customer_id, {{ cents('amount') }} as amount_cents\n\
             from {{ source('public', 'raw_orders') }}",
        );
        template(
            "models/customers.sql",
            "{{ config(materialized='table') }}\n\
             select customer_id, sum(amount_cents) as total_cents\n\
             from {{ ref('stg_orders') }}\n\
             where amount_cents >= {{ var('min_cents', 0) }}\n\
             group by 1;",
        );
    }

    fn count(sql: &str) -> i64 {
        Spi::get_one::<i64>(sql).unwrap().unwrap()
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
    fn compiles_a_model() {
        jaffle_shop();
        let sql = Spi::get_one::<String>("SELECT dbinja_compile('customers', 'analytics')")
            .unwrap()
            .unwrap();
        assert_eq!(
            sql,
            "select customer_id, sum(amount_cents) as total_cents\n\
             from \"analytics\".\"stg_orders\"\n\
             where amount_cents >= 0\n\
             group by 1"
        );
    }

    #[pg_test]
    fn lists_models_in_build_order() {
        jaffle_shop();
        type Row = (i32, String, String, Vec<String>, Vec<String>);
        let rows: Vec<Row> = Spi::connect(|c| {
            c.select(
                "SELECT build_order, model, materialized, depends_on, sources FROM dbinja_models()",
                None,
                &[],
            )
            .unwrap()
            .map(|r| {
                (
                    r.get(1).unwrap().unwrap(),
                    r.get(2).unwrap().unwrap(),
                    r.get(3).unwrap().unwrap(),
                    r.get(4).unwrap().unwrap(),
                    r.get(5).unwrap().unwrap(),
                )
            })
            .collect()
        });
        assert_eq!(
            rows,
            [
                (
                    1,
                    "stg_orders".into(),
                    "view".into(),
                    vec![],
                    vec![r#""public"."raw_orders""#.into()]
                ),
                (
                    2,
                    "customers".into(),
                    "table".into(),
                    vec!["stg_orders".into()],
                    vec![]
                ),
            ]
        );
    }

    #[pg_test]
    fn builds_and_rebuilds_the_project() {
        jaffle_shop();
        let steps = count("SELECT count(*) FROM dbinja_run('analytics')");
        assert_eq!(steps, 2, "first run only creates");
        assert_eq!(
            count("SELECT total_cents FROM analytics.customers WHERE customer_id = 1"),
            1500
        );
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT relkind::text FROM pg_class WHERE oid = 'analytics.stg_orders'::regclass"
            )
            .unwrap()
            .unwrap(),
            "v"
        );

        // A second run drops dependents first, then recreates in order.
        let actions = Spi::get_one::<String>(
            "SELECT string_agg(action || ' ' || model, ', ' ORDER BY step)
               FROM dbinja_run('analytics', '{\"min_cents\": 600}')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            actions,
            "drop customers, drop stg_orders, create stg_orders, create customers"
        );
        assert_eq!(
            count("SELECT total_cents FROM analytics.customers WHERE customer_id = 1"),
            1000
        );
        assert_eq!(
            count("SELECT count(*) FROM dbinja_managed WHERE schema_name = 'analytics'"),
            2
        );
    }

    #[pg_test]
    fn never_replaces_relations_it_did_not_create() {
        jaffle_shop();
        Spi::run("CREATE SCHEMA analytics; CREATE TABLE analytics.customers (precious int)")
            .unwrap();
        let err = error_of("SELECT * FROM dbinja_run('analytics')");
        assert!(err.contains("was not created by dbinja"), "{err}");
        assert_eq!(count("SELECT count(*) FROM information_schema.columns WHERE table_name = 'customers' AND column_name = 'precious'"), 1);
    }

    #[pg_test]
    fn a_failing_model_changes_nothing() {
        jaffle_shop();
        Spi::run("SELECT * FROM dbinja_run('analytics')").unwrap();
        template(
            "models/broken.sql",
            "select * from {{ ref('customers') }} where nope",
        );
        // Different vars, so that customers would hold different totals had
        // its rebuild survived the failure.
        let err = error_of(r#"SELECT * FROM dbinja_run('analytics', '{"min_cents": 600}')"#);
        assert!(err.contains("nope"), "{err}");
        assert_eq!(
            count("SELECT total_cents FROM analytics.customers WHERE customer_id = 1"),
            1500
        );
        assert_eq!(count("SELECT count(*) FROM dbinja_managed"), 2);
    }

    #[pg_test]
    fn dependents_that_are_not_models_block_the_rebuild() {
        jaffle_shop();
        Spi::run("SELECT * FROM dbinja_run('analytics')").unwrap();
        Spi::run("CREATE VIEW public.report AS SELECT * FROM analytics.customers").unwrap();
        let err = error_of("SELECT * FROM dbinja_run('analytics')");
        assert!(err.contains("depend"), "{err}");
        assert_eq!(count("SELECT count(*) FROM public.report"), 2);
    }

    #[pg_test]
    fn reports_compile_errors() {
        template("models/a.sql", "select * from {{ ref('b') }}");
        template("models/b.sql", "select * from {{ ref('a') }}");
        let err = error_of("SELECT * FROM dbinja_models()");
        assert!(err.contains("dependency cycle: a -> b -> a"), "{err}");

        let err = error_of("SELECT dbinja_compile('missing')");
        assert!(err.contains("no enabled model named \"missing\""), "{err}");
    }

    #[pg_test]
    fn rejects_identifiers_postgres_would_truncate() {
        let long = "x".repeat(64);
        template(&format!("models/{long}.sql"), "select 1");
        let err = error_of("SELECT * FROM dbinja_models()");
        assert!(err.contains("63-byte identifier limit"), "{err}");
    }
}
