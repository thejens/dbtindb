"""End-to-end tests of `dbtindb load --dialect duckdb`: a project directory
on disk, loaded through the generated script, then built by the extension.

Usage: as test_duckdb_dbtindb.py, after also building the CLI
       (`cargo build --release -p dbtindb-cli`).
Set DBTINDB_CLI_PATH to test a binary somewhere other than target/release.
"""

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

from test_duckdb_dbtindb import ROOT, SHOP, call, shop, steps

CLI_PATH = os.environ.get(
    "DBTINDB_CLI_PATH", str(ROOT / "target" / "release" / ("dbtindb.exe" if sys.platform == "win32" else "dbtindb"))
)

# Content that breaks naive quoting: quotes, backslashes, and a line that a
# duckdb shell would take for a dot command outside a string.
TRICKY = "select 'it''s' as a, '\\n' as b, '$dbt$' as c\n.tables\n--"


def load(directory, *args):
    out = subprocess.run(
        [CLI_PATH, "load", str(directory), "--dialect", "duckdb", *args],
        capture_output=True, text=True, check=True,
    )
    return out.stdout


def write(directory, files):
    for path, text in files.items():
        p = pathlib.Path(directory, path)
        p.parent.mkdir(parents=True, exist_ok=True)
        # Bytes, so Windows does not turn \n into \r\n.
        p.write_bytes(text.encode())


def paths(conn):
    return [p for (p,) in conn.execute("SELECT path FROM dbtindb_files ORDER BY 1").fetchall()]


class LoadTest(unittest.TestCase):
    def test_loads_and_builds_a_project_from_disk(self):
        conn = shop()
        conn.execute("DROP TABLE dbtindb_files")
        with tempfile.TemporaryDirectory() as d:
            write(d, SHOP)
            write(d, {"models/tricky.sql": TRICKY, "target/x.sql": "nope"})
            # The script creates the table when it is missing.
            conn.execute(load(d))
        self.assertEqual(paths(conn), sorted(list(SHOP) + ["models/tricky.sql"]))
        (source,) = conn.execute("SELECT source FROM dbtindb_files WHERE path = 'models/tricky.sql'").fetchone()
        self.assertEqual(source, TRICKY)
        r = call(conn, "dbtindb_build", json.dumps({"exclude": "tricky"}))
        self.assertIn(("customers", "create table", "success"), steps(r))

    def test_replaces_or_merges(self):
        conn = shop()
        with tempfile.TemporaryDirectory() as d:
            write(d, {"dbt_project.yml": "name: shop\n", "models/a.sql": "select 1 as x"})
            conn.execute(load(d))
            self.assertEqual(paths(conn), ["dbt_project.yml", "models/a.sql"])
            write(d, {"models/b.sql": "select 2 as x"})
            os.remove(pathlib.Path(d, "models/a.sql"))
            conn.execute(load(d, "--merge"))
            self.assertEqual(paths(conn), ["dbt_project.yml", "models/a.sql", "models/b.sql"])
            conn.execute(load(d))
            self.assertEqual(paths(conn), ["dbt_project.yml", "models/b.sql"])


if __name__ == "__main__":
    unittest.main()
