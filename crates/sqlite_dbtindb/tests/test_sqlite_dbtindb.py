"""End-to-end tests of the built SQLite extension, through a stock SQLite.

Parsing and compilation are unit-tested in dbtindb-core; these cover what
only the loaded extension does: building relations, incremental models,
seeds, snapshots, tests, hooks, selection, bookkeeping and rollback.

Usage: python -m unittest discover -s crates/sqlite_dbtindb/tests
       (after `cargo build --release -p sqlite_dbtindb`)
Set SQLITE_DBTINDB_PATH to test a library somewhere other than target/release.
"""

import json
import os
import pathlib
import sqlite3
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[3]
PREFIX = "" if sys.platform == "win32" else "lib"
EXT_PATH = os.environ.get(
    "SQLITE_DBTINDB_PATH", str(ROOT / "target" / "release" / f"{PREFIX}sqlite_dbtindb")
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
{% macro sqlite__label(x) %}'sqlite-{{ x }}'{% endmacro %}
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


def connect():
    # Autocommit, so the tests see exactly the transactions dbtindb makes.
    conn = sqlite3.connect(":memory:", isolation_level=None)
    conn.enable_load_extension(True)
    # No entry point given: SQLite must derive sqlite3_sqlitedbtindb_init.
    conn.load_extension(EXT_PATH)
    conn.enable_load_extension(False)
    conn.execute("CREATE TABLE dbtindb_files (path TEXT PRIMARY KEY, source TEXT NOT NULL)")
    return conn


def put(conn, files):
    conn.executemany("INSERT OR REPLACE INTO dbtindb_files VALUES (?, ?)", files.items())


def shop():
    conn = connect()
    conn.executescript(
        """
        CREATE TABLE orders (id INTEGER, customer_id INTEGER, country TEXT, amount REAL, updated_at TEXT);
        INSERT INTO orders VALUES
          (1, 1, 'SE', 10, '2024-01-01'), (2, 1, 'SE', 5, '2024-01-01'), (3, 2, 'NO', 7, '2024-01-01');
        """
    )
    put(conn, SHOP)
    return conn


def call(conn, fn, options=None):
    (out,) = conn.execute(f"SELECT {fn}(?)", (options,)).fetchone()
    return json.loads(out)


def steps(result):
    return [(s["name"], s["action"], s["status"]) for s in result["steps"]]


def objects(conn):
    return sorted(
        conn.execute(
            "SELECT type, name FROM sqlite_master WHERE type IN ('table', 'view') "
            "AND name NOT IN ('dbtindb_files', 'orders', 'dbtindb_managed')"
        ).fetchall()
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
                # As in dbt build: a node waits for the tests of what it reads.
                ("customer_report", "create view", "success"),
            ],
        )
        self.assertEqual(
            conn.execute("SELECT * FROM customer_report ORDER BY 1").fetchall(),
            [(1, 1500, "Sweden", "sqlite-x"), (2, 700, "Norway", "sqlite-x")],
        )
        # The seed's booleans are 1/0 integers on SQLite.
        self.assertEqual(conn.execute("SELECT code, eu FROM countries ORDER BY 1").fetchall(), [("NO", 0), ("SE", 1)])
        self.assertNotIn(("view", "paid_orders"), objects(conn))
        self.assertFalse(conn.in_transaction)

    def test_rebuild_replaces_and_updates(self):
        conn = shop()
        call(conn, "dbtindb_build")
        r = call(conn, "dbtindb_build", json.dumps({"vars": {"min_cents": 600}}))
        actions = {s["name"]: s["action"] for s in r["steps"]}
        self.assertEqual(actions["countries"], "reload seed")
        self.assertEqual(actions["orders_snapshot"], "update snapshot")
        self.assertEqual(actions["customers"], "create table")
        self.assertEqual(conn.execute("SELECT total_cents FROM customers WHERE customer_id = 1").fetchone(), (1000,))
        self.assertEqual(
            conn.execute("SELECT relation_name, materialized FROM dbtindb_managed ORDER BY 1").fetchall(),
            [("countries", "seed"), ("customer_report", "view"), ("customers", "table"),
             ("orders_snapshot", "snapshot"), ("stg_orders", "view")],
        )

    def test_selection_and_dependent_views(self):
        conn = shop()
        call(conn, "dbtindb_build")
        # Rebuilding only the table also rebuilds the view on top of it.
        r = call(conn, "dbtindb_run", "customers")
        self.assertEqual(steps(r), [("customers", "create table", "success"),
                                    ("customer_report", "create view", "success")])
        r = call(conn, "dbtindb_run", json.dumps({"select": "tag:mart", "exclude": "customer_report"}))
        self.assertEqual([s["name"] for s in r["steps"]], ["customers", "customer_report"])
        r = call(conn, "dbtindb_test", "countries")
        self.assertEqual(steps(r), [("unique_countries_code", "test", "pass")])

    def test_ls_and_compile(self):
        conn = shop()
        nodes = call(conn, "dbtindb_ls", "+customers,resource_type:model +customers,resource_type:seed")
        # Tests whose parents are all selected come along (eager selection).
        self.assertEqual(
            [(n["name"], n["resource_type"], n["materialized"]) for n in nodes],
            [("countries", "seed", "seed"), ("unique_countries_code", "test", None),
             ("stg_orders", "model", "view"), ("paid_orders", "model", "ephemeral"),
             ("customers", "model", "table"), ("accepted_values_customers_country_name", "test", None),
             ("assert_positive_totals", "test", None), ("not_null_customers_customer_id", "test", None),
             ("unique_customers_customer_id", "test", None)],
        )
        self.assertEqual(nodes[4]["depends_on"], ["model.shop.paid_orders", "seed.shop.countries"])
        (sql,) = conn.execute("SELECT dbtindb_compile('customers')").fetchone()
        self.assertTrue(sql.startswith('with "__dbt__cte__paid_orders" as ('), sql)

    def test_inline_sql_and_views(self):
        conn = shop()
        call(conn, "dbtindb_build")
        (sql,) = conn.execute(
            "SELECT dbtindb_sql(?, ?)",
            ("select count(*) from {{ ref('customers') }} where total_cents > {{ var('min') }}", '{"vars": {"min": 1000}}'),
        ).fetchone()
        self.assertEqual(sql, 'select count(*) from "customers" where total_cents > 1000')
        # Ephemeral models are inlined, as in a model.
        (sql,) = conn.execute("SELECT dbtindb_view('paid', 'select * from {{ ref(\"paid_orders\") }}')").fetchone()
        self.assertTrue(sql.startswith('with "__dbt__cte__paid_orders" as ('), sql)
        self.assertEqual(conn.execute("SELECT count(*) FROM paid").fetchone(), (3,))
        self.assertEqual(conn.execute("SELECT type FROM temp.sqlite_master WHERE name = 'paid'").fetchone(), ("view",))
        # A query that fails to render or to parse keeps the previous view.
        with self.assertRaisesRegex(sqlite3.OperationalError, "nope"):
            conn.execute("SELECT dbtindb_view('paid', 'select * from {{ ref(\"nope\") }}')")
        with self.assertRaisesRegex(sqlite3.OperationalError, "syntax error"):
            conn.execute("SELECT dbtindb_view('paid', 'select from from {{ ref(\"customers\") }}')")
        self.assertEqual(conn.execute("SELECT count(*) FROM paid").fetchone(), (3,))
        conn.execute("SELECT dbtindb_view('paid', 'select 1 as a, 2 as b')")
        self.assertEqual(conn.execute("SELECT b FROM paid").fetchone(), (2,))

    def test_failing_tests(self):
        conn = shop()
        # A duplicate order id, and a customer from a country the
        # accepted_values test does not list.
        put(conn, {"seeds/countries.csv": SHOP["seeds/countries.csv"] + "DK,Denmark,true\n"})
        conn.execute("INSERT INTO orders VALUES (3, 3, 'DK', 1, '2024-01-01')")
        r = call(conn, "dbtindb_build", json.dumps({"exclude": "unique_source_raw_orders_id"}))
        statuses = {s["name"]: (s["status"], s["failures"]) for s in r["steps"]}
        # Severity warn: reported, not fatal.
        self.assertEqual(statuses["accepted_values_customers_country_name"], ("warn", 1))
        # An error-severity failure in a build rolls the whole build back.
        with self.assertRaisesRegex(sqlite3.OperationalError, "test unique_source_raw_orders_id failed with 1 failing rows"):
            call(conn, "dbtindb_build", json.dumps({"vars": {"min_cents": 600}}))
        self.assertEqual(conn.execute("SELECT total_cents FROM customers WHERE customer_id = 1").fetchone(), (1500,))
        # dbtindb_test reports failures without raising.
        r = call(conn, "dbtindb_test", "source:raw")
        self.assertIn(("unique_source_raw_orders_id", "test", "fail"), steps(r))

    def test_a_failing_build_changes_nothing(self):
        conn = shop()
        call(conn, "dbtindb_build")
        before = objects(conn)
        put(conn, {"models/marts/broken.sql": "select nope from {{ ref('customers') }}"})
        for begin in (False, True):
            if begin:
                conn.execute("BEGIN")
            with self.assertRaisesRegex(sqlite3.OperationalError, "no such column: nope"):
                call(conn, "dbtindb_run", json.dumps({"vars": {"min_cents": 600}}))
            self.assertEqual(objects(conn), before)
            self.assertEqual(conn.execute("SELECT total_cents FROM customers WHERE customer_id = 1").fetchone(), (1500,))
            self.assertEqual(conn.in_transaction, begin)


class IncrementalTest(unittest.TestCase):
    def project(self, conn, config, body="select id, v from src"):
        put(conn, {"models/inc.sql": "{{ config(materialized='incremental', " + config + ") }}\n" + body
                   + "\n{% if is_incremental() %} where id > (select coalesce(max(id), 0) from {{ this }}){% endif %}"})

    def test_append_and_delete_insert(self):
        conn = connect()
        conn.executescript("CREATE TABLE src (id INTEGER, v TEXT); INSERT INTO src VALUES (1, 'a'), (2, 'b');")
        self.project(conn, "unique_key='id'")
        self.assertEqual(steps(call(conn, "dbtindb_run")), [("inc", "create table", "success")])
        conn.execute("INSERT INTO src VALUES (3, 'c')")
        self.assertEqual(steps(call(conn, "dbtindb_run")), [("inc", "incremental (delete+insert)", "success")])
        self.assertEqual(conn.execute("SELECT * FROM inc ORDER BY id").fetchall(), [(1, "a"), (2, "b"), (3, "c")])

        # Without the filter, delete+insert replaces matching keys.
        put(conn, {"models/inc.sql": "{{ config(materialized='incremental', unique_key='id') }} select id, upper(v) as v from src"})
        call(conn, "dbtindb_run")
        self.assertEqual(conn.execute("SELECT * FROM inc ORDER BY id").fetchall(), [(1, "A"), (2, "B"), (3, "C")])

        # Append duplicates; a full refresh rebuilds the table.
        put(conn, {"models/inc.sql": "{{ config(materialized='incremental') }} select id, v from src where id = 3"})
        self.assertEqual(steps(call(conn, "dbtindb_run")), [("inc", "incremental (append)", "success")])
        self.assertEqual(conn.execute("SELECT count(*) FROM inc").fetchone(), (4,))
        r = call(conn, "dbtindb_run", json.dumps({"full_refresh": True}))
        self.assertEqual(steps(r), [("inc", "create table", "success")])
        self.assertEqual(conn.execute("SELECT * FROM inc").fetchall(), [(3, "c")])

    def test_on_schema_change(self):
        conn = connect()
        conn.executescript("CREATE TABLE src (id INTEGER, v TEXT); INSERT INTO src VALUES (1, 'a');")
        self.project(conn, "on_schema_change='fail'")
        call(conn, "dbtindb_run")
        self.project(conn, "on_schema_change='fail'", "select id, v, 'x' as extra from src")
        with self.assertRaisesRegex(sqlite3.OperationalError, "columns changed"):
            call(conn, "dbtindb_run")
        self.project(conn, "on_schema_change='append_new_columns'", "select id, v, 'x' as extra from src")
        conn.execute("INSERT INTO src VALUES (2, 'b')")
        call(conn, "dbtindb_run")
        self.assertEqual(conn.execute("SELECT * FROM inc ORDER BY id").fetchall(), [(1, "a", None), (2, "b", "x")])


class SnapshotTest(unittest.TestCase):
    def test_timestamp_strategy_and_hard_deletes(self):
        conn = shop()
        call(conn, "dbtindb_snapshot")
        conn.execute("UPDATE orders SET amount = 99, updated_at = '2024-02-01' WHERE id = 1")
        conn.execute("DELETE FROM orders WHERE id = 3")
        conn.execute("INSERT INTO orders VALUES (4, 3, 'SE', 1, '2024-02-01')")
        self.assertEqual(steps(call(conn, "dbtindb_snapshot")), [("orders_snapshot", "update snapshot", "success")])
        rows = conn.execute(
            "SELECT id, amount, dbt_valid_from, dbt_valid_to IS NOT NULL FROM orders_snapshot ORDER BY id, dbt_valid_from"
        ).fetchall()
        self.assertEqual(
            rows,
            [(1, 10.0, "2024-01-01", 1), (1, 99.0, "2024-02-01", 0), (2, 5.0, "2024-01-01", 0),
             (3, 7.0, "2024-01-01", 1), (4, 1.0, "2024-02-01", 0)],
        )
        (closed,) = conn.execute("SELECT dbt_valid_to FROM orders_snapshot WHERE id = 1 AND amount = 10").fetchone()
        self.assertEqual(closed, "2024-02-01")
        # Unchanged source: nothing new.
        call(conn, "dbtindb_snapshot")
        self.assertEqual(conn.execute("SELECT count(*) FROM orders_snapshot").fetchone(), (5,))
        self.assertEqual(conn.execute("SELECT count(DISTINCT dbt_scd_id) FROM orders_snapshot").fetchone(), (5,))

    def test_check_strategy(self):
        conn = connect()
        conn.executescript("CREATE TABLE people (id INTEGER, name TEXT); INSERT INTO people VALUES (1, 'ada');")
        put(conn, {"snapshots/s.sql": "{% snapshot people_snap %}{{ config(unique_key='id', strategy='check', check_cols=['name']) }}"
                                      "select * from people{% endsnapshot %}"})
        call(conn, "dbtindb_snapshot")
        conn.execute("UPDATE people SET name = 'grace'")
        call(conn, "dbtindb_snapshot")
        self.assertEqual(
            conn.execute("SELECT name, dbt_valid_to IS NULL FROM people_snap ORDER BY dbt_valid_from, name").fetchall(),
            [("ada", 0), ("grace", 1)],
        )

    def test_yaml_snapshot_recording_deletes(self):
        conn = connect()
        conn.executescript("CREATE TABLE people (id INTEGER, name TEXT); INSERT INTO people VALUES (1, 'ada'), (2, 'bob');")
        put(conn, {
            "models/people_src.sql": "select * from people",
            "snapshots/people.yml": "snapshots:\n  - name: people_snap\n    relation: ref('people_src')\n    config:\n"
                                    "      unique_key: id\n      strategy: check\n      check_cols: all\n      hard_deletes: new_record\n",
        })
        call(conn, "dbtindb_build")
        conn.execute("DELETE FROM people WHERE id = 2")
        call(conn, "dbtindb_snapshot")
        conn.execute("INSERT INTO people VALUES (2, 'bob')")
        call(conn, "dbtindb_snapshot")
        call(conn, "dbtindb_snapshot")
        rows = conn.execute(
            "SELECT id, dbt_is_deleted, dbt_valid_to IS NULL FROM people_snap ORDER BY id, dbt_valid_from, dbt_is_deleted"
        ).fetchall()
        self.assertEqual(rows, [(1, "False", 1), (2, "False", 0), (2, "True", 0), (2, "False", 1)])
        # The deletion closes the old version exactly when it starts.
        (gap,) = conn.execute(
            "SELECT count(*) FROM people_snap a JOIN people_snap b ON b.id = a.id "
            "WHERE a.dbt_is_deleted = 'False' AND b.dbt_is_deleted = 'True' AND a.dbt_valid_to = b.dbt_valid_from"
        ).fetchone()
        self.assertEqual(gap, 1)


class ContextTest(unittest.TestCase):
    def test_hooks_run_query_and_log(self):
        conn = connect()
        conn.executescript(
            "CREATE TABLE audit (what TEXT); CREATE TABLE src (k TEXT, v INTEGER);"
            "INSERT INTO src VALUES ('a', 1), ('b', 2);"
        )
        put(conn, {
            "dbt_project.yml": "name: p\non-run-start: \"insert into audit values ('start')\"\n"
                               "on-run-end: [\"insert into audit values ('end')\"]\n",
            "models/pivot.sql": """
{{ config(materialized='table', post_hook="insert into audit values ('built {{ this.identifier }}')") }}
{% set keys = run_query('select distinct k from src order by k') %}
{% if execute %}{{ log('pivoting ' ~ keys.columns[0].values() | join(',')) }}{% endif %}
select
{%- for k in (keys.columns[0].values() if execute else []) %}
  sum(case when k = '{{ k }}' then v end) as {{ k }}{{ ',' if not loop.last }}
{%- endfor %}
from src
""",
        })
        r = call(conn, "dbtindb_run")
        self.assertEqual([s["action"] for s in r["steps"]], ["on-run-start", "create table", "on-run-end"])
        self.assertEqual(r["logs"], ["pivoting a,b"])
        self.assertEqual(conn.execute("SELECT * FROM pivot").fetchall(), [(1, 2)])
        self.assertEqual(conn.execute("SELECT what FROM audit").fetchall(), [("start",), ("built pivot",), ("end",)])

    def test_custom_generic_and_singular_tests(self):
        conn = connect()
        put(conn, {
            "macros/tests.sql": "{% test is_even(model, column_name) %}select * from {{ model }} where {{ column_name }} % 2 = 1{% endtest %}",
            "models/m.sql": "select 2 as n union all select 3",
            "models/s.yml": "models:\n  - name: m\n    columns:\n      - name: n\n        tests:\n          - is_even:\n              config: {error_if: '>1', warn_if: '>0'}\n",
            "tests/no_threes.sql": "select * from {{ ref('m') }} where n = 3",
        })
        call(conn, "dbtindb_run")
        statuses = {s["name"]: (s["status"], s["failures"]) for s in call(conn, "dbtindb_test")["steps"]}
        self.assertEqual(statuses, {"is_even_m_n": ("warn", 1), "no_threes": ("fail", 1)})

    def test_store_failures_docs_and_analyses(self):
        conn = connect()
        put(conn, {
            "dbt_project.yml": "name: shop\nmodels:\n  +materialized: table\n",
            "models/m.sql": "select 2 as n union all select 3",
            "models/m.yml": "models:\n  - name: m\n    description: \"{{ doc('m') }}\"\n    columns:\n      - name: n\n"
                            "        tests:\n          - accepted_values:\n              values: [2]\n              quote: false\n"
                            "              config: {store_failures: true}\n",
            "models/docs.md": "{% docs m %}Numbers.{% enddocs %}",
            "analyses/odd.sql": "select n from {{ ref('m') }} where n % 2 = 1",
        })
        call(conn, "dbtindb_run")
        self.assertEqual(steps(call(conn, "dbtindb_test")), [("accepted_values_m_n", "test", "fail")])
        self.assertEqual(conn.execute("SELECT * FROM accepted_values_m_n").fetchall(), [(3, 1)])
        (compiled,) = conn.execute("SELECT dbtindb_compile('odd')").fetchone()
        self.assertEqual(compiled, 'select n from "m" where n % 2 = 1')
        # Analyses are compiled, never built.
        self.assertEqual(objects(conn), [("table", "accepted_values_m_n"), ("table", "m")])


class SafetyTest(unittest.TestCase):
    def test_never_replaces_relations_it_did_not_create(self):
        conn = shop()
        conn.execute("CREATE TABLE Customers (precious INTEGER)")
        with self.assertRaisesRegex(sqlite3.OperationalError, "was not created by dbtindb"):
            call(conn, "dbtindb_build")
        self.assertIn(("table", "Customers"), objects(conn))
        self.assertNotIn(("view", "stg_orders"), objects(conn))

    def test_runs_inside_the_callers_transaction(self):
        conn = shop()
        conn.execute("BEGIN")
        call(conn, "dbtindb_build")
        conn.execute("ROLLBACK")
        self.assertEqual(objects(conn), [])

    def test_cannot_be_used_from_a_view_or_a_model(self):
        conn = shop()
        conn.execute("CREATE VIEW sneaky AS SELECT dbtindb_run()")
        with self.assertRaisesRegex(sqlite3.OperationalError, "unsafe use of dbtindb_run"):
            conn.execute("SELECT * FROM sneaky").fetchall()
        put(conn, {"models/loop.sql": "{{ config(materialized='table') }} select dbtindb_run() as x"})
        with self.assertRaisesRegex(sqlite3.OperationalError, "cannot run while another is running"):
            call(conn, "dbtindb_run")

    def test_reports_errors(self):
        conn = sqlite3.connect(":memory:")
        conn.enable_load_extension(True)
        conn.load_extension(EXT_PATH)
        with self.assertRaisesRegex(sqlite3.OperationalError, "no dbtindb_files table"):
            conn.execute("SELECT dbtindb_ls()").fetchone()
        conn = shop()
        with self.assertRaisesRegex(sqlite3.OperationalError, 'no enabled model, snapshot, test or analysis named "nope"'):
            conn.execute("SELECT dbtindb_compile('nope')").fetchone()
        with self.assertRaisesRegex(sqlite3.OperationalError, "unknown option"):
            call(conn, "dbtindb_ls", '{"selec": "x"}')
        put(conn, {"models/marts/x.sql": "{{ config(schema='other') }} select 1"})
        with self.assertRaisesRegex(sqlite3.OperationalError, "SQLite has no schemas"):
            call(conn, "dbtindb_ls")


if __name__ == "__main__":
    unittest.main()
