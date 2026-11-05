"""End-to-end tests of `dbtindb load`: a project directory on disk, loaded
through the generated script, then built by the extension.

Usage: as test_sqlite_dbtindb.py, after also building the CLI
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

from test_sqlite_dbtindb import ROOT, SHOP, call, steps, shop

CLI_PATH = os.environ.get(
    "DBTINDB_CLI_PATH", str(ROOT / "target" / "release" / ("dbtindb.exe" if sys.platform == "win32" else "dbtindb"))
)

# Content that breaks naive quoting: quotes, backslashes, and a line that a
# sqlite3 shell would take for a dot command outside a string.
TRICKY = "select 'it''s' as a, '\\n' as b, '$dbt$' as c\n.tables\n--"


def load(directory, *args):
    out = subprocess.run(
        [CLI_PATH, "load", str(directory), "--dialect", "sqlite", *args],
        capture_output=True, text=True, check=True,
    )
    return out.stdout, out.stderr


def write(directory, files):
    for path, text in files.items():
        p = pathlib.Path(directory, path)
        p.parent.mkdir(parents=True, exist_ok=True)
        # Bytes, so Windows does not turn \n into \r\n.
        p.write_bytes(text.encode())


class LoadTest(unittest.TestCase):
    def test_loads_and_builds_a_project_from_disk(self):
        conn = shop()
        conn.execute("DELETE FROM dbtindb_files")
        with tempfile.TemporaryDirectory() as d:
            write(d, SHOP)
            write(d, {"models/tricky.sql": TRICKY, "models/notes.md": "# docs blocks live here", "target/x.sql": "nope"})
            script, stderr = load(d)
        conn.executescript(script)
        paths = [p for (p,) in conn.execute("SELECT path FROM dbtindb_files ORDER BY 1")]
        self.assertEqual(paths, sorted(list(SHOP) + ["models/notes.md", "models/tricky.sql"]))
        self.assertEqual(conn.execute("SELECT source FROM dbtindb_files WHERE path = 'models/tricky.sql'").fetchone(), (TRICKY,))
        self.assertIn(f"{len(paths)} files", stderr)
        r = call(conn, "dbtindb_build", json.dumps({"exclude": "tricky"}))
        self.assertIn(("customers", "create table", "success"), steps(r))

    def test_replaces_or_merges(self):
        conn = shop()
        with tempfile.TemporaryDirectory() as d:
            write(d, {"dbt_project.yml": "name: shop\n", "models/a.sql": "select 1 as x"})
            conn.executescript(load(d)[0])
            self.assertEqual([p for (p,) in conn.execute("SELECT path FROM dbtindb_files ORDER BY 1")],
                             ["dbt_project.yml", "models/a.sql"])
            write(d, {"models/b.sql": "select 2 as x"})
            os.remove(pathlib.Path(d, "models/a.sql"))
            conn.execute("INSERT INTO dbtindb_files VALUES ('models/kept.sql', 'select 3')")
            conn.executescript(load(d, "--merge")[0])
            self.assertEqual([p for (p,) in conn.execute("SELECT path FROM dbtindb_files ORDER BY 1")],
                             ["dbt_project.yml", "models/a.sql", "models/b.sql", "models/kept.sql"])
            conn.executescript(load(d)[0])
            self.assertEqual([p for (p,) in conn.execute("SELECT path FROM dbtindb_files ORDER BY 1")],
                             ["dbt_project.yml", "models/b.sql"])

    def test_reports_errors(self):
        with tempfile.TemporaryDirectory() as d:
            out = subprocess.run([CLI_PATH, "load", d, "--dialect", "sqlite"], capture_output=True, text=True)
        self.assertEqual(out.returncode, 1)
        self.assertEqual(out.stdout, "")
        self.assertIn("a dbt project?", out.stderr)


if __name__ == "__main__":
    unittest.main()
