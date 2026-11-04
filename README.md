# dbinja: run dbt models inside PostgreSQL and SQLite

[![CI](https://github.com/thejens/dbinja/actions/workflows/ci.yml/badge.svg)](https://github.com/thejens/dbinja/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![PostgreSQL 14–18 | SQLite 3.37+](https://img.shields.io/badge/PostgreSQL%2014–18%20%7C%20SQLite%203.37%2B-lightgrey)

**dbinja runs dbt-style SQL models inside the database.**
You store the models as rows, and `SELECT dbinja_run()` compiles them and builds every view and table in dependency order. There is no Python, no dbt CLI and no orchestrator to run.

```sql
INSERT INTO jinja_templates (name, source) VALUES
  ('models/staging/stg_orders.sql',
   $$select id, customer_id, amount * 100 as amount_cents
     from {{ source('public', 'raw_orders') }}$$),
  ('models/customers.sql',
   $${{ config(materialized='table') }}
     select customer_id, sum(amount_cents) as total_cents
     from {{ ref('stg_orders') }}
     group by 1$$);

SELECT * FROM dbinja_run('analytics');
--  step |   model    | action |                 statement
-- ------+------------+--------+-------------------------------------------
--     1 | stg_orders | create | CREATE VIEW "analytics"."stg_orders" AS ...
--     2 | customers  | create | CREATE TABLE "analytics"."customers" AS ...
```

- **dbt's own Jinja dialect:** models are rendered with the MiniJinja fork from [dbt v2](https://github.com/dbt-labs/dbt), the Rust engine that dbt itself uses, and not a lookalike.
- **The dbt functions that shape a DAG:** `ref`, `source`, `config`, `var`, `this`, `target` and `is_incremental`, plus project-wide macros from `macros/`.
- **Atomic builds:** a run happens in one transaction. If any model fails, nothing changes.
- **Safe rebuilds:** dbinja only drops relations it created, never uses `CASCADE`, and refuses to replace a table or view it did not create.
- **Lineage in SQL:** `dbinja_models()` lists each model's dependencies, sources and build order. `dbinja_compile()` shows the SQL a model compiles to.
- **Two databases, one project:** the same model files build in PostgreSQL (as an extension built with [pgrx](https://github.com/pgcentralfoundation/pgrx)) and in SQLite (as a loadable extension).

dbinja is the third piece of a small family. It reads its models from the `jinja_templates` table of [pgjinja](https://github.com/thejens/pgjinja) (PostgreSQL) or [sqlite-jinja](https://github.com/thejens/sqlite-jinja) (SQLite), so the same rows can also be rendered with `jinja_render_named()`.

## Contents

- [Why build dbt models inside the database?](#why-build-dbt-models-inside-the-database)
- [Installation](#installation)
- [Writing a project](#writing-a-project)
- [PostgreSQL](#postgresql)
- [SQLite](#sqlite)
- [What is supported from dbt](#what-is-supported-from-dbt)
- [How a run works](#how-a-run-works)
- [FAQ](#faq)
- [Development](#development)

## Why build dbt models inside the database?

dbt is the standard way to manage SQL transformations, but running it means having a Python or Rust CLI, a profile with credentials, and a scheduler somewhere that can reach the database. For many projects that is more machinery than the transformations need:

- **An app's own database:** derived tables and reporting views that ship with the application and rebuild with one SQL call, from a migration, a cron job (`pg_cron`) or the app itself.
- **SQLite files:** analytics databases, Datasette instances and local-first apps that carry their models inside the `.sqlite` file.
- **Locked-down environments:** where the only way in is a SQL connection.
- **Prototyping:** you can try a dbt model in `psql` without setting up a project.

dbinja is not a replacement for dbt on a warehouse. It builds views and tables, the core of most dbt projects. It does not run tests, snapshots, seeds or incremental models (see [what is supported](#what-is-supported-from-dbt)).

## Installation

dbinja is built from source with a [Rust toolchain](https://rustup.rs). The first build fetches dbt's Jinja engine from [dbt-labs/dbt](https://github.com/dbt-labs/dbt), pinned to the v2.0.5 release.

**PostgreSQL 14–18:** install [pgjinja](https://github.com/thejens/pgjinja#installation) first, then:

```sh
cargo install cargo-pgrx --version 0.19.3 --locked
cargo pgrx init --pg16 "$(which pg_config)"   # once; use your Postgres version

git clone https://github.com/thejens/dbinja.git
cd dbinja/crates/pg_dbinja
cargo pgrx install --release --no-default-features --features pg16
```

```sql
CREATE EXTENSION pg_dbinja CASCADE;   -- CASCADE also creates pgjinja
```

**SQLite 3.37.2 or newer:** this also needs libclang, which is used to generate the SQLite bindings.

```sh
git clone https://github.com/thejens/dbinja.git
cd dbinja
cargo build --release -p sqlite_dbinja
# target/release/libsqlite_dbinja.so (.dylib on macOS, sqlite_dbinja.dll on Windows)
```

```sql
.load ./target/release/libsqlite_dbinja
```

## Writing a project

A project is a set of rows in `jinja_templates`, named like the files of a dbt project:

| Name | What it is |
|---|---|
| `models/**/<name>.sql` | A model. It is built as a view or table called `<name>`, and other models `ref('<name>')` it. Directories are only for organisation, and model names must be unique across them, as in dbt. |
| `macros/**/*.sql` | Macros. Every model can call them without an import. |
| anything else | Available to `{% include %}`, `{% import %}` and `{% from ... import %}` by name. |

A model is one `SELECT` (a trailing `;` is allowed):

```sql
-- models/marts/customer_orders.sql
{{ config(materialized='table') }}

select
    c.customer_id,
    count(*)            as orders,
    {{ cents('sum(o.amount)') }} as total_cents
from {{ ref('stg_customers') }} c
join {{ ref('stg_orders') }} o using (customer_id)
where o.ordered_at >= '{{ var("since", "2020-01-01") }}'
group by 1
```

```sql
-- macros/money.sql
{% macro cents(expr) %}round({{ expr }} * 100)::bigint{% endmacro %}
```

To load an existing dbt project from disk, insert each file under its path relative to the project root. With the `sqlite3` CLI, which has a `readfile()` function:

```sh
cd my_dbt_project
find models macros -name '*.sql' | while read -r f; do
  sqlite3 ../app.db "INSERT OR REPLACE INTO jinja_templates VALUES ('$f', CAST(readfile('$f') AS TEXT))"
done
```

## PostgreSQL

| Function | Returns | Description |
|---|---|---|
| `dbinja_run(target_schema text = 'public', vars jsonb = '{}')` | `TABLE(step, model, action, statement)` | Builds every enabled model into `target_schema` and returns the statements it ran. Creates the schema if needed. |
| `dbinja_compile(model text, target_schema text = 'public', vars jsonb = '{}')` | `text` | The SQL `model` compiles to, as `dbt compile` would show it. |
| `dbinja_models(target_schema text = 'public', vars jsonb = '{}')` | `TABLE(build_order, model, path, materialized, relation, depends_on text[], sources text[])` | The enabled models in build order, with their lineage. |

```sql
-- Build into a staging schema with different vars, check it, then go live.
SELECT * FROM dbinja_run('analytics_staging', '{"since": "2024-01-01"}');

-- Which models read from raw.orders?
SELECT model FROM dbinja_models() WHERE '"raw"."orders"' = ANY (sources);

-- Rebuild nightly with pg_cron.
SELECT cron.schedule('dbinja', '0 3 * * *', $$SELECT dbinja_run('analytics')$$);
```

`ref()` renders as `"<target_schema>"."<model>"` and `source('raw', 'orders')` as `"raw"."orders"`. Model names longer than PostgreSQL's 63-byte identifier limit are rejected, because Postgres would silently truncate them.

Relations built by dbinja are recorded in `dbinja_managed` (in the extension's schema), which `pg_dump` includes.

## SQLite

SQLite functions return a single value, so list results are JSON arrays that you can query with `json_each()`. Models are always built in the `main` schema, because a SQLite view can only reference objects in its own schema. `ref()` therefore renders as `"<model>"`, and `source('main', 'orders')` as `"orders"`.

| Function | Returns |
|---|---|
| `dbinja_run([vars])` | A JSON array of `{step, model, action, statement}` |
| `dbinja_compile(model [, vars])` | The compiled SQL |
| `dbinja_models([vars])` | A JSON array of `{build_order, model, path, materialized, relation, depends_on, sources}` |

`vars` is JSON text, for example `'{"since": "2024-01-01"}'`.

```sql
CREATE TABLE jinja_templates (name TEXT PRIMARY KEY, source TEXT NOT NULL);
-- ... insert models ...

SELECT json_extract(value, '$.model'), json_extract(value, '$.action')
FROM json_each(dbinja_run());

SELECT json_extract(value, '$.model') AS model, json_extract(value, '$.depends_on')
FROM json_each(dbinja_models());
```

The functions are `DIRECTONLY`: they can be called from your SQL but not from views or triggers, where anyone who can change the schema could make other users run them. Built relations are recorded in `main.dbinja_managed`.

SQLite only checks a view's columns when the view is queried, so a model built as a view that references a missing column builds fine and fails on first use. Models built as tables fail during the run.

## What is supported from dbt

| | |
|---|---|
| `ref('model')` | ✅ Builds the dependency graph and renders the relation. Two-argument (cross-project) `ref` is not supported. |
| `source('schema', 'table')` | ✅ Without `sources.yml`, the source name is the schema. Recorded as lineage. |
| `config(materialized=...)` | ✅ `'view'` (default) and `'table'`. |
| `config(enabled=false)` | ✅ The model is skipped, and a `ref` to it is an error, as in dbt. |
| `config(tags=, meta=, docs=, description=, persist_docs=, group=, access=)` | Accepted and ignored. |
| Other `config()` keys (`schema`, `alias`, `unique_key`, ...) | ❌ An error rather than a silently different build. |
| `var('name', default)` | ✅ From the `vars` argument. |
| `this`, `target.type`, `target.schema` | ✅ |
| `is_incremental()` | Always `false`: every run is a full rebuild, as with `dbt run --full-refresh`. |
| Macros in `macros/` | ✅ Global, as in dbt. A macro name defined twice is an error. |
| Jinja: `{% set %}`, loops, filters, `{% call %}`, whitespace control, ... | ✅ dbt's MiniJinja fork. |
| Incremental, snapshot, ephemeral and materialized-view materializations | ❌ |
| Tests, seeds, hooks, `dbt_project.yml`, packages, adapter macros (`adapter.*`, `run_query`) | ❌ |

On PostgreSQL, pgjinja's `jinja_templates` table checks each template with standard Jinja2 syntax. dbt-only block tags such as `{% docs %}` and `{% materialization %}` cannot be stored there. Model and macro files never need them.

## How a run works

1. Every template is read from `jinja_templates`, and every model is rendered. Rendering records the model's `ref`s, `source`s and `config`.
2. The models are sorted topologically. A cycle is reported with its path, for example `dependency cycle: a -> c -> b -> a`.
3. Every model relation that already exists is dropped, dependents first. Then every model is created in dependency order with `CREATE VIEW|TABLE ... AS <compiled SQL>`.
4. All of this runs in one transaction: the caller's transaction on PostgreSQL, and a savepoint on SQLite. Any error rolls back the whole run.

Rebuilds are deliberately conservative:

- dbinja only drops relations listed in its `dbinja_managed` table. If a table you created stands where a model would go, the run fails instead of replacing it.
- Drops never cascade. On PostgreSQL, a view of yours that depends on a model makes the rebuild fail, so it is never silently dropped. Drop it, or make it a model too.
- Renders have an instruction budget, so a runaway macro cannot pin a database backend.

## FAQ

### Is this dbt?

No. dbinja uses dbt's Jinja engine and follows dbt's conventions for models, macros, `ref` and `source`, so dbt models that stick to [the supported features](#what-is-supported-from-dbt) compile unchanged. It does not use dbt's parser, adapters, manifest or CLI, and it is not affiliated with dbt Labs.

### Why not just run dbt?

You should if you already run dbt, or if you need tests, snapshots, incremental models or a warehouse. dbinja is for cases where an external process is the problem: models that ship inside an application database or a SQLite file, rebuilds triggered from SQL or `pg_cron`, and environments where you only have a SQL connection.

### Can I use it on Amazon RDS, Cloud SQL, Supabase or Neon?

Only where you can install custom extensions, which managed services generally do not allow. It works on self-hosted PostgreSQL, in Docker images, and anywhere SQLite can load extensions.

### Who can run a build?

On PostgreSQL, `dbinja_run` runs with the caller's privileges. It needs `CREATE` on the target schema, `SELECT` on the sources and on `jinja_templates`, and write access to the bookkeeping table:

```sql
GRANT SELECT, INSERT, DELETE ON dbinja_managed TO builder;
```

 Anyone who can write to `jinja_templates` decides what SQL the next run executes, so grant that like DDL rights.

## Development

```sh
# Core (compilation, ordering, planning)
cargo test -p dbinja-core

# PostgreSQL extension; installs pgjinja into the pgrx-managed Postgres first
cargo pgrx test pg16 -p pg_dbinja

# SQLite extension, end to end through Python's sqlite3
cargo build --release -p sqlite_dbinja
python3 -m unittest discover -s crates/sqlite_dbinja/tests -v
```

The workspace has three crates. `dbinja-core` compiles models, orders them and plans the statements, and is shared by both databases. `pg_dbinja` and `sqlite_dbinja` read the project from the database and execute the plan.

## License

MIT. dbt's MiniJinja fork, a dependency, is licensed under Apache 2.0.
