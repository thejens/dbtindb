"""End-to-end tests of the built SQLite extension, through a stock SQLite.

Compilation itself is unit-tested in dbinja-core; these cover what only the
loaded extension does: reading jinja_templates, building relations, the
bookkeeping table and rollback.

Usage: python -m unittest discover -s crates/sqlite_dbinja/tests
       (after `cargo build --release -p sqlite_dbinja`)
Set SQLITE_DBINJA_PATH to test a library somewhere other than target/release.
"""

import json
import os
import pathlib
import sqlite3
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[3]
SUFFIX = {"darwin": "dylib", "win32": "dll"}.get(sys.platform, "so")
PREFIX = "" if sys.platform == "win32" else "lib"
EXT_PATH = os.environ.get(
    "SQLITE_DBINJA_PATH", str(ROOT / "target" / "release" / f"{PREFIX}sqlite_dbinja")
)

PROJECT = {
    "macros/cents.sql": "{% macro cents(col) %}cast({{ col }} * 100 as integer){% endmacro %}",
    "models/staging/stg_orders.sql": (
        "select id, customer_id, {{ cents('amount') }} as amount_cents\n"
        "from {{ source('main', 'raw_orders') }}"
    ),
    "models/customers.sql": (
        "{{ config(materialized='table') }}\n"
        "select customer_id, sum(amount_cents) as total_cents\n"
        "from {{ ref('stg_orders') }}\n"
        "where amount_cents >= {{ var('min_cents', 0) }}\n"
        "group by 1;"
    ),
}


def connect():
    # Autocommit, so the tests see exactly the transactions dbinja makes.
    conn = sqlite3.connect(":memory:", isolation_level=None)
    conn.enable_load_extension(True)
    # No entry point given: SQLite must derive sqlite3_sqlitedbinja_init.
    conn.load_extension(EXT_PATH)
    conn.enable_load_extension(False)
    return conn


def jaffle_shop():
    conn = connect()
    conn.executescript(
        """
        CREATE TABLE jinja_templates (name TEXT PRIMARY KEY, source TEXT NOT NULL);
        CREATE TABLE raw_orders (id INTEGER, customer_id INTEGER, amount REAL);
        INSERT INTO raw_orders VALUES (1, 1, 10), (2, 1, 5), (3, 2, 7);
        """
    )
    conn.executemany("INSERT INTO jinja_templates VALUES (?, ?)", PROJECT.items())
    return conn


def run(conn, vars=None):
    (out,) = conn.execute("SELECT dbinja_run(?)", (vars,)).fetchone()
    return json.loads(out)


def objects(conn):
    return conn.execute(
        "SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY name"
    ).fetchall()


class SqliteDbinjaTest(unittest.TestCase):
    def test_compiles_a_model(self):
        conn = jaffle_shop()
        (sql,) = conn.execute("SELECT dbinja_compile('customers')").fetchone()
        self.assertEqual(
            sql,
            "select customer_id, sum(amount_cents) as total_cents\n"
            'from "stg_orders"\n'
            "where amount_cents >= 0\n"
            "group by 1",
        )

    def test_lists_models_in_build_order(self):
        conn = jaffle_shop()
        rows = conn.execute(
            "SELECT m.value ->> 'model', m.value ->> 'materialized', m.value -> 'depends_on' "
            "FROM json_each(dbinja_models()) AS m ORDER BY m.value ->> 'build_order'"
            if sqlite3.sqlite_version_info >= (3, 38)
            else "SELECT json_extract(m.value, '$.model'), json_extract(m.value, '$.materialized'), "
            "json_extract(m.value, '$.depends_on') FROM json_each(dbinja_models()) AS m"
        ).fetchall()
        self.assertEqual(
            rows,
            [("stg_orders", "view", "[]"), ("customers", "table", '["stg_orders"]')],
        )

    def test_builds_and_rebuilds_the_project(self):
        conn = jaffle_shop()
        steps = run(conn)
        self.assertEqual([(s["action"], s["model"]) for s in steps],
                         [("create", "stg_orders"), ("create", "customers")])
        self.assertIn(("view", "stg_orders"), objects(conn))
        self.assertIn(("table", "customers"), objects(conn))
        self.assertEqual(
            conn.execute("SELECT total_cents FROM customers WHERE customer_id = 1").fetchone(),
            (1500,),
        )

        steps = run(conn, '{"min_cents": 600}')
        self.assertEqual(
            [(s["action"], s["model"]) for s in steps],
            [("drop", "customers"), ("drop", "stg_orders"),
             ("create", "stg_orders"), ("create", "customers")],
        )
        self.assertEqual(
            conn.execute("SELECT total_cents FROM customers WHERE customer_id = 1").fetchone(),
            (1000,),
        )
        self.assertEqual(
            conn.execute("SELECT relation_name, path, materialized FROM dbinja_managed "
                         "ORDER BY 1").fetchall(),
            [("customers", "models/customers.sql", "table"),
             ("stg_orders", "models/staging/stg_orders.sql", "view")],
        )
        self.assertFalse(conn.in_transaction)

    def test_never_replaces_relations_it_did_not_create(self):
        conn = jaffle_shop()
        conn.execute("CREATE TABLE Customers (precious INTEGER)")
        with self.assertRaisesRegex(sqlite3.OperationalError, "was not created by dbinja"):
            run(conn)
        self.assertIn(("table", "Customers"), objects(conn))
        self.assertNotIn(("view", "stg_orders"), objects(conn))

    def test_a_failing_model_changes_nothing(self):
        conn = jaffle_shop()
        run(conn)
        before = objects(conn)
        conn.execute(
            "INSERT INTO jinja_templates VALUES "
            "('models/broken.sql', '{{ config(materialized=''table'') }} "
            "select nope from {{ ref(''customers'') }}')"
        )
        # A table, because SQLite only resolves a view's columns when the
        # view is queried. Different vars, so that customers would hold
        # different totals had its rebuild survived the failure.
        for begin in (False, True):
            if begin:
                conn.execute("BEGIN")
            with self.assertRaisesRegex(sqlite3.OperationalError, "no such column: nope"):
                run(conn, '{"min_cents": 600}')
            self.assertEqual(objects(conn), before)
            self.assertEqual(
                conn.execute("SELECT total_cents FROM customers WHERE customer_id = 1").fetchone(),
                (1500,),
            )
            self.assertEqual(conn.execute("SELECT count(*) FROM dbinja_managed").fetchone(), (2,))
            self.assertEqual(conn.in_transaction, begin)

    def test_runs_inside_the_callers_transaction(self):
        conn = jaffle_shop()
        conn.execute("BEGIN")
        run(conn)
        conn.execute("ROLLBACK")
        self.assertNotIn(("view", "stg_orders"), objects(conn))
        self.assertNotIn(("table", "dbinja_managed"), objects(conn))

    def test_cannot_be_used_from_a_view(self):
        conn = jaffle_shop()
        conn.execute("CREATE VIEW sneaky AS SELECT dbinja_run()")
        with self.assertRaisesRegex(sqlite3.OperationalError, "unsafe use of dbinja_run"):
            conn.execute("SELECT * FROM sneaky").fetchall()

    def test_a_model_cannot_run_the_build(self):
        conn = jaffle_shop()
        conn.execute(
            "INSERT INTO jinja_templates VALUES "
            "('models/loop.sql', '{{ config(materialized=''table'') }} select dbinja_run() as x')"
        )
        with self.assertRaisesRegex(sqlite3.OperationalError, "cannot be called while it is running"):
            run(conn)
        self.assertNotIn(("view", "stg_orders"), objects(conn))

    def test_reports_errors(self):
        conn = connect()
        with self.assertRaisesRegex(sqlite3.OperationalError, "no jinja_templates table"):
            conn.execute("SELECT dbinja_models()").fetchone()
        conn = jaffle_shop()
        with self.assertRaisesRegex(sqlite3.OperationalError, 'no enabled model named "nope"'):
            conn.execute("SELECT dbinja_compile('nope')").fetchone()
        with self.assertRaisesRegex(sqlite3.OperationalError, "vars must be a JSON object"):
            conn.execute("SELECT dbinja_models('[1]')").fetchone()


if __name__ == "__main__":
    unittest.main()
