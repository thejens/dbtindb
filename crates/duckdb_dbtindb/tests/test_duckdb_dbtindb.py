"""End-to-end tests of the packaged DuckDB extension, loaded from Python.

Parsing and compilation are unit-tested in dbtindb-core; these cover what
only the loaded extension does: building relations, incremental models,
seeds, snapshots, tests, hooks, selection, bookkeeping and rollback.

Usage: python -m unittest discover -s crates/duckdb_dbtindb/tests
       (after `cargo build --release -p duckdb_dbtindb` and
       `python3 crates/duckdb_dbtindb/package.py ...`)
Set DUCKDB_DBTINDB_PATH to test an extension somewhere other than
target/duckdb/dbtindb.duckdb_extension.
"""

import datetime
import json
import os
import pathlib
import unittest

import duckdb

ROOT = pathlib.Path(__file__).resolve().parents[3]
EXT_PATH = os.environ.get(
    "DUCKDB_DBTINDB_PATH", str(ROOT / "target" / "duckdb" / "dbtindb.duckdb_extension")
)

SHOP = {
    "dbt_project.yml": """
name: shop
vars:
  min_cents: 0
models:
  shop:
    marts:
      +materialized: table
      +tags: [mart]
""",
    "macros/money.sql": "{% macro cents(col) %}cast({{ col }} * 100 as integer){% endmacro %}",
    "macros/dispatch.sql": """
{% macro label(x) %}{{ return(adapter.dispatch('label')(x)) }}{% endmacro %}
{% macro default__label(x) %}'default-{{ x }}'{% endmacro %}
{% macro duckdb__label(x) %}'duckdb-{{ x }}'{% endmacro %}
""",
    "seeds/countries.csv": "code,name,eu\nSE,Sweden,true\nNO,Norway,false\n",
    "models/staging/stg_orders.sql": """
select id, customer_id, country, {{ cents('amount') }} as amount_cents, updated_at
from {{ source('raw', 'orders') }}
""",
    "models/staging/paid_orders.sql": """
{{ config(materialized='ephemeral') }}
select * from {{ ref('stg_orders') }} where amount_cents > {{ var('min_cents') }}
""",
    "models/marts/customers.sql": """
select o.customer_id, sum(o.amount_cents) as total_cents, max(c.name) as country_name,
       {{ label('x') }} as label
from {{ ref('paid_orders') }} o
left join {{ ref('countries') }} c on c.code = o.country
group by 1
""",
    "models/marts/customer_report.sql": """
{{ config(materialized='view') }}
select * from {{ ref('customers') }}
""",
    "models/schema.yml": """
version: 2
sources:
  - name: raw
    schema: main
    tables:
      - name: orders
        columns:
          - name: id
            data_tests: [not_null, unique]
models:
  - name: customers
    columns:
      - name: customer_id
        data_tests: [unique, not_null]
      - name: country_name
        data_tests:
          - accepted_values:
              values: [Sweden, Norway]
              config: {severity: warn}
seeds:
  - name: countries
    columns:
      - name: code
        data_tests: [unique]
""",
    "snapshots/orders_snapshot.sql": """
{% snapshot orders_snapshot %}
{{ config(unique_key='id', strategy='timestamp', updated_at='updated_at', invalidate_hard_deletes=True) }}
select id, customer_id, amount, updated_at from {{ source('raw', 'orders') }}
{% endsnapshot %}
""",
    "tests/assert_positive_totals.sql": "select * from {{ ref('customers') }} where total_cents < 0",
}

FILES_DDL = "CREATE TABLE dbtindb_files (path VARCHAR PRIMARY KEY, source VARCHAR NOT NULL)"


def connect(path=":memory:"):
    # The extension is unsigned, which DuckDB refuses unless told otherwise.
    conn = duckdb.connect(path, config={"allow_unsigned_extensions": True})
    conn.execute(f"LOAD '{EXT_PATH}'")
    conn.execute(FILES_DDL)
    return conn


def put(conn, files):
    conn.executemany("INSERT OR REPLACE INTO dbtindb_files VALUES (?, ?)", list(files.items()))


def shop():
    conn = connect()
    conn.execute(
        """
        CREATE TABLE orders (id INTEGER, customer_id INTEGER, country VARCHAR, amount DOUBLE, updated_at DATE);
        INSERT INTO orders VALUES
          (1, 1, 'SE', 10, '2024-01-01'), (2, 1, 'SE', 5, '2024-01-01'), (3, 2, 'NO', 7, '2024-01-01');
        """
    )
    put(conn, SHOP)
    return conn


def call(conn, fn, options=None):
    if options is None:
        (out,) = conn.execute(f"SELECT {fn}()").fetchone()
    else:
        (out,) = conn.execute(f"SELECT {fn}(?)", [options]).fetchone()
    return json.loads(out)


def steps(result):
    return [(s["name"], s["action"], s["status"]) for s in result["steps"]]


def rows(conn, sql, params=None):
    return conn.execute(sql, params).fetchall()


def objects(conn):
    return rows(
        conn,
        "SELECT table_type, table_name FROM information_schema.tables "
        "WHERE table_name NOT IN ('dbtindb_files', 'orders', 'dbtindb_managed') ORDER BY 2",
    )


class BuildTest(unittest.TestCase):
    def test_build_runs_everything_in_dag_order(self):
        conn = shop()
        r = call(conn, "dbtindb_build")
        self.assertEqual(
            steps(r),
            [
                ("countries", "create seed", "success"),
                ("not_null_source_raw_orders_id", "test", "pass"),
                ("unique_countries_code", "test", "pass"),
                ("unique_source_raw_orders_id", "test", "pass"),
                ("stg_orders", "create view", "success"),
                ("customers", "create table", "success"),
                ("orders_snapshot", "create snapshot", "success"),
                ("accepted_values_customers_country_name", "test", "pass"),
                ("assert_positive_totals", "test", "pass"),
                ("not_null_customers_customer_id", "test", "pass"),
                ("unique_customers_customer_id", "test", "pass"),
                ("customer_report", "create view", "success"),
            ],
        )
        self.assertEqual(
            rows(conn, "SELECT * FROM customer_report ORDER BY 1"),
            [(1, 1500, "Sweden", "duckdb-x"), (2, 700, "Norway", "duckdb-x")],
        )
        self.assertEqual(rows(conn, "SELECT code, eu FROM countries ORDER BY 1"), [("NO", False), ("SE", True)])
        self.assertNotIn(("VIEW", "paid_orders"), objects(conn))

    def test_rebuild_replaces_and_updates(self):
        conn = shop()
        call(conn, "dbtindb_build")
        r = call(conn, "dbtindb_build", json.dumps({"vars": {"min_cents": 600}}))
        actions = {s["name"]: s["action"] for s in r["steps"]}
        self.assertEqual(actions["countries"], "reload seed")
        self.assertEqual(actions["orders_snapshot"], "update snapshot")
        self.assertEqual(actions["customers"], "create table")
        self.assertEqual(rows(conn, "SELECT total_cents FROM customers WHERE customer_id = 1"), [(1000,)])
        self.assertEqual(
            rows(conn, "SELECT relation_name, materialized FROM dbtindb_managed ORDER BY 1"),
            [("countries", "seed"), ("customer_report", "view"), ("customers", "table"),
             ("orders_snapshot", "snapshot"), ("stg_orders", "view")],
        )

    def test_options_as_selection_json_or_struct(self):
        conn = shop()
        call(conn, "dbtindb_build")
        r = call(conn, "dbtindb_run", "customers")
        self.assertEqual(steps(r), [("customers", "create table", "success"),
                                    ("customer_report", "create view", "success")])
        (out,) = conn.execute(
            "SELECT dbtindb_run({'select': 'tag:mart', 'exclude': 'customer_report'})"
        ).fetchone()
        # Views on a rebuilt table are rebuilt with it, excluded or not.
        self.assertEqual([s["name"] for s in json.loads(out)["steps"]], ["customers", "customer_report"])
        (out,) = conn.execute("SELECT dbtindb_test('countries') ->> '$.steps[0].name'").fetchone()
        self.assertEqual(out, "unique_countries_code")

    def test_schemas(self):
        conn = shop()
        put(conn, {"models/marts/other.sql": "{{ config(schema='reporting') }} select 1 as one"})
        call(conn, "dbtindb_run", "other")
        self.assertEqual(rows(conn, "SELECT one FROM main_reporting.other"), [(1,)])

    def test_ls_compile_and_inline_sql(self):
        conn = shop()
        nodes = call(conn, "dbtindb_ls", "+customers,resource_type:model")
        self.assertEqual([n["name"] for n in nodes][:3], ["stg_orders", "paid_orders", "customers"])
        (sql,) = conn.execute("SELECT dbtindb_compile('customers')").fetchone()
        self.assertTrue(sql.startswith('with "__dbt__cte__paid_orders" as ('), sql)
        call(conn, "dbtindb_build")
        (sql,) = conn.execute(
            "SELECT dbtindb_sql(?, ?)",
            ["select count(*) from {{ ref('customers') }} where total_cents > {{ var('min') }}",
             '{"vars": {"min": 1000}}'],
        ).fetchone()
        self.assertEqual(sql, 'select count(*) from "main"."customers" where total_cents > 1000')
        # DuckDB's query() runs rendered SQL directly.
        self.assertEqual(
            rows(conn, "SELECT * FROM query(dbtindb_sql('select count(*) from {{ ref(\"paid_orders\") }}'))"),
            [(3,)],
        )

    def test_failing_tests_roll_the_build_back(self):
        conn = shop()
        put(conn, {"seeds/countries.csv": SHOP["seeds/countries.csv"] + "DK,Denmark,true\n"})
        conn.execute("INSERT INTO orders VALUES (3, 3, 'DK', 1, '2024-01-01')")
        r = call(conn, "dbtindb_build", json.dumps({"exclude": "unique_source_raw_orders_id"}))
        statuses = {s["name"]: (s["status"], s["failures"]) for s in r["steps"]}
        self.assertEqual(statuses["accepted_values_customers_country_name"], ("warn", 1))
        with self.assertRaisesRegex(duckdb.Error, "test unique_source_raw_orders_id failed with 1 failing rows"):
            call(conn, "dbtindb_build", json.dumps({"vars": {"min_cents": 600}}))
        self.assertEqual(rows(conn, "SELECT total_cents FROM customers WHERE customer_id = 1"), [(1500,)])
        r = call(conn, "dbtindb_test", "source:raw")
        self.assertIn(("unique_source_raw_orders_id", "test", "fail"), steps(r))

    def test_a_failing_build_changes_nothing(self):
        conn = shop()
        call(conn, "dbtindb_build")
        before = objects(conn)
        put(conn, {"models/marts/broken.sql": "select nope from {{ ref('customers') }}"})
        with self.assertRaisesRegex(duckdb.Error, '"nope" not found'):
            call(conn, "dbtindb_run", json.dumps({"vars": {"min_cents": 600}}))
        self.assertEqual(objects(conn), before)
        self.assertEqual(rows(conn, "SELECT total_cents FROM customers WHERE customer_id = 1"), [(1500,)])


class IncrementalTest(unittest.TestCase):
    def project(self, conn, config, body="select id, v from src"):
        put(conn, {"models/inc.sql": "{{ config(materialized='incremental', " + config + ") }}\n" + body
                   + "\n{% if is_incremental() %} where id > (select coalesce(max(id), 0) from {{ this }}){% endif %}"})

    def test_strategies(self):
        conn = connect()
        conn.execute("CREATE TABLE src (id INTEGER, v VARCHAR); INSERT INTO src VALUES (1, 'a'), (2, 'b');")
        self.project(conn, "unique_key='id'")
        self.assertEqual(steps(call(conn, "dbtindb_run")), [("inc", "create table", "success")])
        conn.execute("INSERT INTO src VALUES (3, 'c')")
        self.assertEqual(steps(call(conn, "dbtindb_run")), [("inc", "incremental (delete+insert)", "success")])
        self.assertEqual(rows(conn, "SELECT * FROM inc ORDER BY id"), [(1, "a"), (2, "b"), (3, "c")])

        put(conn, {"models/inc.sql": "{{ config(materialized='incremental', unique_key='id', "
                                     "incremental_strategy='merge') }} select id, upper(v) as v from src"})
        self.assertEqual(steps(call(conn, "dbtindb_run")), [("inc", "incremental (merge)", "success")])
        self.assertEqual(rows(conn, "SELECT * FROM inc ORDER BY id"), [(1, "A"), (2, "B"), (3, "C")])

        put(conn, {"models/inc.sql": "{{ config(materialized='incremental') }} select id, v from src where id = 3"})
        self.assertEqual(steps(call(conn, "dbtindb_run")), [("inc", "incremental (append)", "success")])
        self.assertEqual(rows(conn, "SELECT count(*) FROM inc"), [(4,)])
        r = call(conn, "dbtindb_run", json.dumps({"full_refresh": True}))
        self.assertEqual(steps(r), [("inc", "create table", "success")])
        self.assertEqual(rows(conn, "SELECT * FROM inc"), [(3, "c")])

    def test_on_schema_change(self):
        conn = connect()
        conn.execute("CREATE TABLE src (id INTEGER, v VARCHAR); INSERT INTO src VALUES (1, 'a');")
        self.project(conn, "on_schema_change='fail'")
        call(conn, "dbtindb_run")
        self.project(conn, "on_schema_change='fail'", "select id, v, 'x' as extra from src")
        with self.assertRaisesRegex(duckdb.Error, "columns changed"):
            call(conn, "dbtindb_run")
        self.project(conn, "on_schema_change='append_new_columns'", "select id, v, 'x' as extra from src")
        conn.execute("INSERT INTO src VALUES (2, 'b')")
        call(conn, "dbtindb_run")
        self.assertEqual(rows(conn, "SELECT * FROM inc ORDER BY id"), [(1, "a", None), (2, "b", "x")])


class SnapshotTest(unittest.TestCase):
    def test_timestamp_strategy_and_hard_deletes(self):
        conn = shop()
        call(conn, "dbtindb_snapshot")
        conn.execute("UPDATE orders SET amount = 99, updated_at = '2024-02-01' WHERE id = 1")
        conn.execute("DELETE FROM orders WHERE id = 3")
        conn.execute("INSERT INTO orders VALUES (4, 3, 'SE', 1, '2024-02-01')")
        self.assertEqual(steps(call(conn, "dbtindb_snapshot")), [("orders_snapshot", "update snapshot", "success")])
        jan, feb = datetime.date(2024, 1, 1), datetime.date(2024, 2, 1)
        self.assertEqual(
            rows(conn, "SELECT id, amount, dbt_valid_from::date, dbt_valid_to IS NOT NULL "
                       "FROM orders_snapshot ORDER BY id, dbt_valid_from"),
            [(1, 10.0, jan, True), (1, 99.0, feb, False), (2, 5.0, jan, False),
             (3, 7.0, jan, True), (4, 1.0, feb, False)],
        )
        call(conn, "dbtindb_snapshot")
        self.assertEqual(rows(conn, "SELECT count(*), count(DISTINCT dbt_scd_id) FROM orders_snapshot"), [(5, 5)])

    def test_check_strategy(self):
        conn = connect()
        conn.execute("CREATE TABLE people (id INTEGER, name VARCHAR); INSERT INTO people VALUES (1, 'ada');")
        put(conn, {"snapshots/s.sql": "{% snapshot people_snap %}{{ config(unique_key='id', strategy='check', check_cols=['name']) }}"
                                      "select * from people{% endsnapshot %}"})
        call(conn, "dbtindb_snapshot")
        conn.execute("UPDATE people SET name = 'grace'")
        call(conn, "dbtindb_snapshot")
        self.assertEqual(
            rows(conn, "SELECT name, dbt_valid_to IS NULL FROM people_snap ORDER BY dbt_valid_from, name"),
            [("ada", False), ("grace", True)],
        )


class ContextTest(unittest.TestCase):
    def test_hooks_run_query_log_and_comments(self):
        conn = connect()
        conn.execute(
            "CREATE TABLE audit (what VARCHAR); CREATE TABLE src (k VARCHAR, v INTEGER);"
            "INSERT INTO src VALUES ('a', 1), ('b', 2);"
        )
        put(conn, {
            "dbt_project.yml": "name: p\non-run-start: \"insert into audit values ('start')\"\n"
                               "on-run-end: [\"insert into audit values ('end')\"]\n"
                               "models:\n  +persist_docs: {relation: true, columns: true}\n",
            "models/pivoted.sql": """
{{ config(materialized='table', post_hook="insert into audit values ('built {{ this.identifier }}')") }}
{% set keys = run_query('select distinct k from src order by k') %}
{% if execute %}{{ log('pivoting ' ~ keys.columns[0].values() | join(',')) }}{% endif %}
select
{%- for k in (keys.columns[0].values() if execute else []) %}
  sum(case when k = '{{ k }}' then v end) as {{ k }}{{ ',' if not loop.last }}
{%- endfor %}
from src
""",
            "models/pivoted.yml": "models:\n  - name: pivoted\n    description: Pivoted.\n"
                                "    columns:\n      - name: a\n        description: The a's.\n",
        })
        r = call(conn, "dbtindb_run")
        self.assertEqual([s["action"] for s in r["steps"]], ["on-run-start", "create table", "on-run-end"])
        self.assertEqual(r["logs"], ["pivoting a,b"])
        self.assertEqual(rows(conn, "SELECT * FROM pivoted"), [(1, 2)])
        self.assertEqual(rows(conn, "SELECT what FROM audit"), [("start",), ("built pivoted",), ("end",)])
        self.assertEqual(rows(conn, "SELECT comment FROM duckdb_tables() WHERE table_name = 'pivoted'"), [("Pivoted.",)])
        self.assertEqual(
            rows(conn, "SELECT comment FROM duckdb_columns() WHERE table_name = 'pivoted' AND column_name = 'a'"),
            [("The a's.",)],
        )

    def test_custom_generic_singular_and_unit_tests(self):
        conn = connect()
        put(conn, {
            "macros/tests.sql": "{% test is_even(model, column_name) %}select * from {{ model }} where {{ column_name }} % 2 = 1{% endtest %}",
            "models/m.sql": "select 2 as n union all select 3",
            "models/s.yml": "models:\n  - name: m\n    columns:\n      - name: n\n        tests:\n          - is_even:\n              config: {error_if: '>1', warn_if: '>0'}\n",
            "tests/no_threes.sql": "select * from {{ ref('m') }} where n = 3",
            "models/unit.yml": "unit_tests:\n  - name: both\n    model: m\n    given: []\n"
                               "    expect:\n      rows: [{n: 2}, {n: 3}]\n",
        })
        call(conn, "dbtindb_run")
        statuses = {s["name"]: (s["status"], s["failures"]) for s in call(conn, "dbtindb_test")["steps"]}
        self.assertEqual(statuses, {"is_even_m_n": ("warn", 1), "no_threes": ("fail", 1), "both": ("pass", 0)})


class SafetyTest(unittest.TestCase):
    def test_never_replaces_relations_it_did_not_create(self):
        conn = shop()
        conn.execute("CREATE TABLE Customers (precious INTEGER)")
        with self.assertRaisesRegex(duckdb.Error, "was not created by dbtindb"):
            call(conn, "dbtindb_build")
        self.assertIn(("BASE TABLE", "Customers"), objects(conn))
        self.assertNotIn(("VIEW", "stg_orders"), objects(conn))

    def test_commits_on_its_own(self):
        # The extension works through its own connection, so the build is
        # its own transaction: rolling back the caller's keeps it.
        conn = shop()
        conn.execute("BEGIN")
        call(conn, "dbtindb_build")
        conn.execute("ROLLBACK")
        self.assertIn(("BASE TABLE", "customers"), objects(conn))

    def test_cannot_be_used_from_a_model(self):
        conn = shop()
        put(conn, {"models/loop.sql": "{{ config(materialized='table') }} select dbtindb_run() as x"})
        with self.assertRaisesRegex(duckdb.Error, "another dbtindb call is running"):
            call(conn, "dbtindb_run")

    def test_reports_errors(self):
        conn = duckdb.connect(config={"allow_unsigned_extensions": True})
        conn.execute(f"LOAD '{EXT_PATH}'")
        with self.assertRaisesRegex(duckdb.Error, "no dbtindb_files table"):
            conn.execute("SELECT dbtindb_ls()").fetchone()
        conn = shop()
        with self.assertRaisesRegex(duckdb.Error, 'no enabled model, snapshot, test, unit test or analysis named "nope"'):
            conn.execute("SELECT dbtindb_compile('nope')").fetchone()
        with self.assertRaisesRegex(duckdb.Error, "unknown option"):
            call(conn, "dbtindb_ls", '{"selec": "x"}')
        put(conn, {"models/mv.sql": "{{ config(materialized='materialized_view') }} select 1"})
        with self.assertRaisesRegex(duckdb.Error, "DuckDB has no materialized views"):
            call(conn, "dbtindb_run", "mv")


if __name__ == "__main__":
    unittest.main()
