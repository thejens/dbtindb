"""End-to-end tests of packages and dbt's cross-database macros on DuckDB.

Usage: as test_duckdb_dbtindb.py.
"""

import datetime
import hashlib
import unittest

from test_duckdb_dbtindb import call, connect, put


def value(conn, sql):
    """Renders `sql` with dbtindb_sql and runs it: one value."""
    (compiled,) = conn.execute("SELECT dbtindb_sql(?)", [sql]).fetchone()
    return conn.execute(compiled).fetchone()[0]


class PackageTest(unittest.TestCase):
    def test_package_models_seeds_and_unit_tests(self):
        conn = connect()
        put(conn, {
            "dbt_packages/audit/dbt_project.yml": "name: audit\nmodels:\n  audit:\n    +materialized: table\n",
            "dbt_packages/audit/seeds/kinds.csv": "id,kind\n1,a\n2,b\n",
            "dbt_packages/audit/models/events.sql":
                "select id, kind, {{ tag() }} as tag from {{ ref('kinds') }}",
            "dbt_packages/audit/models/s.yml":
                "models:\n  - name: events\n    columns: [{name: id, data_tests: [unique]}]\n",
            "dbt_packages/audit/macros/tag.sql": "{% macro tag() %}{{ return(adapter.dispatch('tag', 'audit')()) }}{% endmacro %}\n"
                                                "{% macro default__tag() %}'default'{% endmacro %}\n"
                                                "{% macro duckdb__tag() %}'audit'{% endmacro %}",
            "models/report.sql": "select count(*) as n, max(tag) as tag from {{ ref('audit', 'events') }}",
            "tests/fixtures/events.csv": "id,kind,tag\n1,a,x\n2,b,y\n3,c,z\n",
            "models/unit.yml": "unit_tests:\n  - name: counts\n    model: report\n"
                               "    given: [{input: \"ref('audit', 'events')\", format: csv, fixture: events}]\n"
                               "    expect: {rows: [{n: 3, tag: z}]}\n",
        })
        result = call(conn, "dbtindb_build")
        self.assertEqual(
            {s["name"]: s["status"] for s in result["steps"]},
            {"kinds": "success", "events": "success", "unique_events_id": "pass",
             "report": "success", "counts": "pass"},
        )
        self.assertEqual(conn.execute("SELECT n, tag FROM report").fetchone(), (2, "audit"))


class CrossDatabaseMacroTest(unittest.TestCase):
    def test_dates_follow_postgres(self):
        conn = connect()
        ts = datetime.datetime
        cases = {
            "dbt.dateadd('month', 1, \"date '2024-01-31'\")": ts(2024, 2, 29),
            "dbt.dateadd('month', -1, \"timestamp '2024-03-31 10:00:00'\")": ts(2024, 2, 29, 10),
            "dbt.dateadd('year', 1, \"date '2024-02-29'\")": ts(2025, 2, 28),
            "dbt.dateadd('hour', 2, \"timestamp '2024-02-28 23:00:00'\")": ts(2024, 2, 29, 1),
            "dbt.datediff(\"date '2024-01-31'\", \"date '2024-03-01'\", 'month')": 2,
            "dbt.datediff(\"timestamp '2024-01-01 23:00:00'\", \"timestamp '2024-01-02 01:00:00'\", 'hour')": 2,
            # As on PostgreSQL, date_trunc gives a timestamp.
            "dbt.date_trunc('week', \"date '2024-02-15'\")": ts(2024, 2, 12),
            "dbt.date_trunc('quarter', \"date '2024-05-15'\")": ts(2024, 4, 1),
            "dbt.last_day(\"date '2024-02-10'\", 'month')": datetime.date(2024, 2, 29),
            "dbt.date(2024, 2, 9)": datetime.date(2024, 2, 9),
        }
        for expr, expected in cases.items():
            with self.subTest(expr):
                self.assertEqual(value(conn, f"select {{{{ {expr} }}}}"), expected)

    def test_strings_and_aggregates(self):
        conn = connect()
        conn.execute("CREATE TABLE t (s VARCHAR, b BOOLEAN)")
        conn.execute("INSERT INTO t VALUES ('x', false), ('y', true)")
        self.assertEqual(value(conn, "select {{ dbt.split_part(\"'a-b-c'\", \"'-'\", 2) }}"), "b")
        self.assertEqual(value(conn, "select {{ dbt.split_part(\"'a-b-c'\", \"'-'\", -1) }}"), "c")
        self.assertEqual(value(conn, "select {{ dbt.hash(\"'x'\") }}"), hashlib.md5(b"x").hexdigest())
        self.assertEqual(value(conn, "select {{ dbt.right(\"'abc'\", 2) }}"), "bc")
        self.assertEqual(value(conn, "select {{ dbt.position(\"'b'\", \"'abc'\") }}"), 2)
        self.assertEqual(value(conn, "select {{ dbt.listagg('s', \"','\", 'order by s desc') }} from t"), "y,x")
        self.assertEqual(value(conn, "select {{ dbt.listagg('s', \"','\", 'order by s', 1) }} from t"), "x")
        self.assertEqual(value(conn, "select {{ dbt.bool_or('b') }} from t"), True)
        self.assertEqual(value(conn, "select {{ dbt.any_value('s') }} from t where b"), "y")
        self.assertEqual(value(conn, "select {{ dbt.cast_bool_to_text('b') }} from t where b"), "true")
        self.assertEqual(value(conn, "select {{ dbt.array_append(dbt.array_construct([1]), 2) }}"), [1, 2])
        self.assertEqual(value(conn, "select count(*) from ({{ dbt.generate_series(10) }})"), 10)
        self.assertEqual(
            value(conn, "select max(date_day) from ({{ dbt.date_spine('day', \"date '2024-01-01'\", \"date '2024-02-01'\") }})"),
            datetime.datetime(2024, 1, 31),
        )
        # A TIMESTAMP WITH TIME ZONE, read as a naive one so the test needs no pytz.
        self.assertIsInstance(value(conn, "select ({{ dbt.current_timestamp() }})::timestamp"), datetime.datetime)


if __name__ == "__main__":
    unittest.main()
