"""End-to-end tests of packages and dbt's cross-database macros on SQLite.

Usage: as test_sqlite_dbtindb.py.
"""

import hashlib
import unittest

from test_sqlite_dbtindb import call, connect, put

# A package in the shape of dbt_utils: a dispatched macro that inspects a
# relation's columns, a package-internal helper, and a generic test with
# fail_calc.
TOOLS = {
    "dbt_packages/tools/dbt_project.yml": "name: tools\nmacro-paths: [macros]\n",
    "dbt_packages/tools/macros/cols.sql": """
{% macro col_list(relation) %}{{ return(adapter.dispatch('col_list', 'tools')(relation)) }}{% endmacro %}
{% macro default__col_list(relation) -%}
  {%- set cols = adapter.get_columns_in_relation(relation) -%}
  {{ join_quoted(cols | map(attribute='name') | list) }}
{%- endmacro %}
""",
    "dbt_packages/tools/macros/helpers.sql": """
{% macro join_quoted(names) %}'{{ names | join(',') }}'{% endmacro %}
""",
    "dbt_packages/tools/macros/tests.sql": """
{% test sums_to(model, column_name, total) %}
{{ config(fail_calc='sum(diff)') }}
select abs(sum({{ column_name }}) - {{ total }}) as diff from {{ model }}
{% endtest %}
""",
}


def value(conn, sql):
    """Renders `sql` with dbtindb_sql and runs it: one value."""
    (compiled,) = conn.execute("SELECT dbtindb_sql(?)", (sql,)).fetchone()
    return conn.execute(compiled).fetchone()[0]


class PackageTest(unittest.TestCase):
    def test_package_macros_tests_and_overrides(self):
        conn = connect()
        put(conn, TOOLS)
        put(conn, {
            "models/base.sql": "select 1 as id, 2 as amount",
            "models/cols.sql": "select {{ tools.col_list(ref('base')) }} as cols",
            "models/s.yml": "models:\n  - name: base\n    columns:\n      - name: amount\n"
                            "        data_tests: [{tools.sums_to: {total: 2}}]\n",
        })
        result = call(conn, "dbtindb_build")
        self.assertEqual(
            {s["name"]: s["status"] for s in result["steps"]},
            {"base": "success", "cols": "success", "tools_sums_to_base_amount": "pass"},
        )
        self.assertEqual(conn.execute("SELECT cols FROM cols").fetchone(), ("id,amount",))

        # The project overrides the dispatched implementation, not the
        # package's internal helper.
        put(conn, {
            "macros/over.sql": "{% macro sqlite__col_list(r) %}{{ join_quoted(['mine']) }}{% endmacro %}\n"
                               "{% macro join_quoted(n) %}'root'{% endmacro %}",
        })
        call(conn, "dbtindb_run")
        self.assertEqual(conn.execute("SELECT cols FROM cols").fetchone(), ("root",))

        put(conn, {
            "models/s.yml": "models:\n  - name: base\n    columns:\n      - name: amount\n"
                            "        data_tests: [{tools.sums_to: {total: 5}}]\n",
        })
        (step,) = call(conn, "dbtindb_test")["steps"]
        self.assertEqual((step["status"], step["failures"]), ("fail", 3))


class CrossDatabaseMacroTest(unittest.TestCase):
    def test_dates_follow_postgres(self):
        conn = connect()
        cases = {
            "dbt.dateadd('month', 1, \"'2024-01-31'\")": "2024-02-29",
            "dbt.dateadd('month', -1, \"'2024-03-31 10:00:00'\")": "2024-02-29 10:00:00",
            "dbt.dateadd('year', 1, \"'2024-02-29'\")": "2025-02-28",
            "dbt.dateadd('day', 1, \"'2024-02-28 23:00:00'\")": "2024-02-29 23:00:00",
            "dbt.dateadd('hour', 2, \"'2024-02-28 23:00:00'\")": "2024-02-29 01:00:00",
            "dbt.datediff(\"'2024-01-31'\", \"'2024-03-01'\", 'month')": 2,
            "dbt.datediff(\"'2024-01-01 23:00:00'\", \"'2024-01-02 01:00:00'\", 'hour')": 2,
            "dbt.datediff(\"'2024-02-18'\", \"'2024-02-19'\", 'week')": 0,
            "dbt.date_trunc('week', \"'2024-02-15'\")": "2024-02-12",
            "dbt.date_trunc('quarter', \"'2024-05-15'\")": "2024-04-01",
            "dbt.last_day(\"'2024-02-10'\", 'month')": "2024-02-29",
            "dbt.date(2024, 2, 9)": "2024-02-09",
        }
        for expr, expected in cases.items():
            with self.subTest(expr):
                self.assertEqual(value(conn, f"select {{{{ {expr} }}}}"), expected)

    def test_strings_and_aggregates(self):
        conn = connect()
        conn.execute("CREATE TABLE t (s TEXT, b INTEGER)")
        conn.execute("INSERT INTO t VALUES ('x', 0), ('y', 1)")
        self.assertEqual(value(conn, "select {{ dbt.split_part(\"'a-b-c'\", \"'-'\", 2) }}"), "b")
        self.assertEqual(value(conn, "select {{ dbt.split_part(\"'a-b-c'\", \"'-'\", -1) }}"), "c")
        self.assertEqual(value(conn, "select {{ dbt.split_part(\"'a-b-c'\", \"'-'\", 9) }}"), "")
        self.assertIsNone(value(conn, "select {{ dbt.split_part('null', \"'-'\", 1) }}"))
        self.assertEqual(value(conn, "select {{ dbt.hash(\"'x'\") }}"), hashlib.md5(b"x").hexdigest())
        self.assertEqual(value(conn, "select {{ dbt.hash('12') }}"), hashlib.md5(b"12").hexdigest())
        self.assertEqual(value(conn, "select {{ dbt.right(\"'abc'\", 2) }}"), "bc")
        self.assertEqual(value(conn, "select {{ dbt.position(\"'b'\", \"'abc'\") }}"), 2)
        self.assertEqual(value(conn, "select {{ dbt.listagg('s', \"','\") }} from t"), "x,y")
        self.assertEqual(value(conn, "select {{ dbt.bool_or('b') }} from t"), 1)
        self.assertEqual(value(conn, "select {{ dbt.cast_bool_to_text('b') }} from t where b"), "true")
        self.assertEqual(value(conn, "select count(*) from ({{ dbt.generate_series(10) }})"), 10)
        self.assertEqual(
            value(conn, "select max(date_day) from ({{ dbt.date_spine('day', \"'2024-01-01'\", \"'2024-02-01'\") }})"),
            "2024-01-31",
        )


if __name__ == "__main__":
    unittest.main()
