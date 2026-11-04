# dbtindb: run dbt projects inside PostgreSQL and SQLite

[![CI](https://github.com/thejens/dbtindb/actions/workflows/ci.yml/badge.svg)](https://github.com/thejens/dbtindb/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![PostgreSQL 14–18 | SQLite 3.37+](https://img.shields.io/badge/PostgreSQL%2014–18%20%7C%20SQLite%203.37%2B-lightgrey)

**dbtindb is dbt inside the database.** You store a dbt project's files as rows. Then `SELECT * FROM dbtindb_build()` seeds, builds, snapshots and tests it in DAG order, in one transaction. There is no Python, no dbt CLI, no profiles.yml and no orchestrator.

```sql
INSERT INTO dbtindb_files (path, source) VALUES
  ('models/staging/stg_orders.sql',
   $$select id, customer_id, amount * 100 as amount_cents
     from {{ source('raw', 'orders') }}$$),
  ('models/marts/customers.sql',
   $${{ config(materialized='table') }}
     select customer_id, sum(amount_cents) as total_cents
     from {{ ref('stg_orders') }}
     group by 1$$),
  ('models/schema.yml',
   $$sources:
       - name: raw
         tables: [{name: orders}]
     models:
       - name: customers
         columns:
           - name: customer_id
             data_tests: [unique, not_null]$$);

SELECT step, name, action, status FROM dbtindb_build(target_schema => 'analytics');
--  step |             name               |    action    | status
-- ------+--------------------------------+--------------+---------
--     1 | stg_orders                     | create view  | success
--     2 | customers                      | create table | success
--     3 | not_null_customers_customer_id | test         | pass
--     4 | unique_customers_customer_id   | test         | pass
```

- **dbt's own Jinja engine:** templates are rendered with the MiniJinja fork from [dbt v2](https://github.com/dbt-labs/dbt), the Rust engine that dbt itself uses, and not a lookalike.
- **dbt's commands:** `build`, `run`, `seed`, `snapshot`, `test`, `ls` and `compile`, with dbt's node selection syntax (`+orders`, `tag:nightly`, `path:models/marts`, `@customers`, `config.materialized:table`, ...).
- **dbt's materializations:** views, tables, ephemeral models, incremental models (append, delete+insert and merge, with `on_schema_change`), materialized views, seeds and snapshots.
- **dbt's tests:** `unique`, `not_null`, `accepted_values`, `relationships`, your own `{% test %}` blocks and singular tests, with `severity`, `warn_if`, `error_if` and `where`.
- **dbt project files:** `dbt_project.yml` with folder-level `+config`, property YAML files, `vars`, hooks, `generate_schema_name`/`generate_alias_name` overrides and `adapter.dispatch`.
- **Atomic builds:** a command runs in one transaction. If a model or an error-severity test fails, nothing changes.
- **Safe rebuilds:** dbtindb only replaces relations it created and never uses `CASCADE`. Your own views on top of a model block its rebuild instead of disappearing.
- **Ad-hoc dbt SQL from psql and sqlite3:** `SELECT dbtindb_sql($$select * from {{ ref('customers') }}$$) \gexec` runs a dbt-templated query as if you had typed the compiled SQL.
- **A loader CLI:** `dbtindb load ./my_project --dialect postgres | psql` copies a dbt project from disk into the database.
- **Two databases, one project:** the same files build in PostgreSQL (as an extension built with [pgrx](https://github.com/pgcentralfoundation/pgrx)) and in SQLite (as a loadable extension).

dbtindb is the dbt-shaped sibling of [pgjinja](https://github.com/thejens/pgjinja) and [sqlite-jinja](https://github.com/thejens/sqlite-jinja), which render plain Jinja2 templates in SQL.

## Contents

- [Why run dbt inside the database?](#why-run-dbt-inside-the-database)
- [Installation](#installation)
- [Writing a project](#writing-a-project)
- [Packages](#packages)
- [Commands](#commands)
- [Ad-hoc dbt SQL](#ad-hoc-dbt-sql)
- [PostgreSQL](#postgresql)
- [SQLite](#sqlite)
- [What is supported from dbt](#what-is-supported-from-dbt)
- [How a command runs](#how-a-command-runs)
- [FAQ](#faq)
- [Development](#development)

## Why run dbt inside the database?

dbt is the standard way to manage SQL transformations, but running it means having a CLI, a profile with credentials, and a scheduler somewhere that can reach the database. For many projects that is more machinery than the transformations need:

- **An app's own database:** derived tables, reporting views and reference data that ship with the application and rebuild with one SQL call, from a migration, a cron job (`pg_cron`) or the app itself.
- **SQLite files:** analytics databases, Datasette instances and local-first apps that carry their dbt project inside the `.sqlite` file.
- **Locked-down environments:** where the only way in is a SQL connection.
- **Prototyping:** you can try a dbt model in `psql` without setting up a project.

dbtindb is not a replacement for dbt on a warehouse. It covers the parts of dbt that a single database needs, and refuses the rest loudly (see [what is supported](#what-is-supported-from-dbt)).

## Installation

dbtindb is built from source with a [Rust toolchain](https://rustup.rs). The first build fetches dbt's Jinja engine from [dbt-labs/dbt](https://github.com/dbt-labs/dbt), pinned to the v2.0.5 release.

**PostgreSQL 14–18:**

```sh
cargo install cargo-pgrx --version 0.19.3 --locked
cargo pgrx init --pg16 "$(which pg_config)"   # once; use your Postgres version

git clone https://github.com/thejens/dbtindb.git
cd dbtindb/crates/pg_dbtindb
cargo pgrx install --release --no-default-features --features pg16
```

```sql
CREATE EXTENSION pg_dbtindb;
```

**SQLite 3.37.2 or newer:** this also needs libclang, which is used to generate the SQLite bindings.

```sh
git clone https://github.com/thejens/dbtindb.git
cd dbtindb
cargo build --release -p sqlite_dbtindb
# target/release/libsqlite_dbtindb.so (.dylib on macOS, sqlite_dbtindb.dll on Windows)
```

```sql
.load ./target/release/libsqlite_dbtindb
```

**The `dbtindb` CLI**, which loads projects from disk:

```sh
cargo install --path crates/dbtindb-cli   # from the clone; installs `dbtindb`
```

## Writing a project

A project is the set of rows in `dbtindb_files`, one per file, keyed by its path in a dbt project:

| Path | What it is |
|---|---|
| `dbt_project.yml` | Optional. `name`, `vars`, the `*-paths` settings (including `analysis-paths` and `docs-paths`), `packages-install-path`, `dispatch`, `on-run-start`/`on-run-end`, and `models:`/`seeds:`/`snapshots:`/`data_tests:` config trees with `+key` settings per folder. |
| `models/**/<name>.sql` | A model, built as `<name>` (or its `alias`). Model names are unique across folders, as in dbt. |
| `models/**/*.yml` | Properties: sources, descriptions, columns, tests and `config:` for models, seeds, snapshots and analyses, YAML snapshots, and `unit_tests:`. |
| `seeds/**/<name>.csv` | A seed, loaded into a table with inferred column types (integer, numeric, boolean, date, timestamp, text) or `column_types`. |
| `snapshots/**/*.sql` | `{% snapshot %}` blocks. Snapshots can also be declared in YAML with `relation:`. |
| `analyses/**/<name>.sql` | An analysis: compiled with `dbtindb_compile`, never run. |
| `**/*.md` | `{% docs %}` blocks, which descriptions use through `{{ doc('name') }}`. They are read from every resource path, or from `docs-paths`. |
| `tests/**/*.sql` | Singular tests. `tests/generic/*.sql` holds `{% test %}` blocks. |
| `macros/**/*.sql` | Macros and `{% test %}` blocks. Every file can call them without an import. |
| `dbt_packages/<package>/**` | An installed package, as `dbt deps` leaves it. See [Packages](#packages). |

Other files are ignored. A model is one `SELECT` (a trailing `;` is allowed):

```sql
-- models/marts/customer_orders.sql
{{ config(materialized='incremental', unique_key='customer_id') }}

select customer_id, count(*) as orders, {{ cents('sum(amount)') }} as total_cents, max(ordered_at) as last_order
from {{ ref('stg_orders') }}
{% if is_incremental() %}
where customer_id in (select customer_id from {{ ref('stg_orders') }} where ordered_at > (select max(last_order) from {{ this }}))
{% endif %}
group by 1
```

### Loading a project from disk

`dbtindb load` reads a dbt project directory and prints a SQL script that loads it, which you pipe into the database's own client. The CLI never connects to a database, so connection strings, `.pgpass`, SSH tunnels and SQLite paths work as they always do:

```sh
dbtindb load ./jaffle_shop --dialect postgres | psql -v ON_ERROR_STOP=1 "$DATABASE_URL"
dbtindb load ./jaffle_shop --dialect sqlite | sqlite3 app.db
```

It takes `dbt_project.yml` and the `.sql`, `.yml`, `.yaml`, `.csv` and `.md` files under the project's model, seed, snapshot, test, analysis, macro and docs paths (as `dbt_project.yml` sets them), and the same for every package in `dbt_packages/`, skipping hidden files, `target/` and anything else. Run `dbt deps` first if the project has packages. The script runs in one transaction. By default it replaces the table's contents, so files deleted on disk disappear from the database; `--merge` only inserts and updates. `--table` loads into a schema-qualified table when the extension's schema is not on the `search_path`. The PostgreSQL script uses dollar quoting, so backslashes, quotes and `:psql_variables` in your files arrive unchanged.

You can also write the rows yourself: a path is a file's path relative to the project root. The PostgreSQL extension creates `dbtindb_files`; on SQLite, `dbtindb load` creates it, or you run `CREATE TABLE dbtindb_files (path TEXT PRIMARY KEY, source TEXT NOT NULL)`.

### Packages

Packages installed by `dbt deps` are read from `dbt_packages/` (or `packages-install-path`), each with its own `dbt_project.yml`. dbtindb uses their macros and generic tests, which is what utility packages such as [dbt_utils](https://github.com/dbt-labs/dbt-utils), dbt_date, dbt_expectations and codegen consist of. A package that defines models, seeds or snapshots is an error.

Macros resolve as in dbt:

- `dbt_utils.star(...)` calls a package's macro, and a test written `dbt_utils.unique_combination_of_columns` in a property file is that package's `{% test %}` block.
- Inside a package, a bare macro name means the package's own macro first, then the project's, then dbt's.
- `adapter.dispatch('star', 'dbt_utils')` searches your project before the package, so a `postgres__star` or `default__star` of yours overrides the package's. A `dispatch:` entry in `dbt_project.yml` sets another search order.
- Vars scoped to a package (`vars: {dbt_utils: {...}}`) apply, and a package's own `vars` are defaults.

dbt's own cross-database macros are built in, as the `dbt` package: `dbt.dateadd`, `dbt.datediff`, `dbt.date_trunc`, `dbt.last_day`, `dbt.split_part`, `dbt.listagg`, `dbt.hash`, `dbt.concat`, `dbt.safe_cast`, `dbt.type_string` and the other `type_*` macros, `dbt.current_timestamp`, `dbt.date_spine`, `dbt.generate_series`, `statement` and `get_columns_in_query`. On PostgreSQL they are dbt-postgres's own. On SQLite, dbtindb implements them on SQLite's date functions:

- Dates stay `'YYYY-MM-DD'` text and timestamps `'YYYY-MM-DD HH:MM:SS'` text.
- Month arithmetic stops at the end of the month, as in PostgreSQL.
- `split_part` and `hash` use functions the extension registers.

Pass dates to them as text literals (`"'2024-01-01'"`), not `cast(... as date)`, which SQLite turns into a number. Arrays are not available on SQLite.

dbt_utils 1.3 works on PostgreSQL. On SQLite, its date, string and test macros work, but `union_relations` and `get_relations_by_pattern` do not: they use SQL SQLite lacks (parenthesised `UNION` members, `information_schema`).

## Commands

| Command | dbt equivalent | What it does |
|---|---|---|
| `dbtindb_build` | `dbt build` | Seeds, models, snapshots, tests and unit tests in DAG order. Each node runs after the tests of the nodes it reads, and an error-severity test failure or a failing unit test aborts the build. |
| `dbtindb_run` | `dbt run` | Models |
| `dbtindb_seed` | `dbt seed` | Seeds |
| `dbtindb_snapshot` | `dbt snapshot` | Snapshots |
| `dbtindb_test` | `dbt test` | Tests and unit tests. Failures are reported as rows with status `fail`, and nothing is raised. |
| `dbtindb_ls` | `dbt ls` | The selected nodes in build order, with lineage and relations |
| `dbtindb_compile` | `dbt compile` | A model's, snapshot's, test's or analysis's compiled SQL. For a unit test, the model's SQL reading its fixtures. |

Every command takes a selector (`--select`), an exclusion (`--exclude`), `vars` and `full_refresh`. Each step reports `step, unique_id, name, resource_type, action, status, failures, sql`. Here `action` is what was done (`create table`, `incremental (merge)`, `reload seed`, `update snapshot`, `refresh materialized view`, `test`, ...), and `sql` is every statement that ran for the node, hooks included.

Selection follows dbt: space-separated items are a union and commas intersect. Graph operators are `+`, `n+`, `+n` and `@`. The selector methods are `fqn` (the default), `path`/`file`, `tag`, `resource_type`, `package`, `test_type` (`generic`, `singular`, `data` or `unit`), `test_name`, `config.<key>` and `source`. Tests whose parents are all selected are included, as with dbt's default eager indirect selection.

## Ad-hoc dbt SQL

`dbtindb_sql(sql)` renders any SQL with the project's `ref`, `source`, macros and `var`s, like `dbt compile --inline`. Ephemeral models become CTEs. psql's `\gexec` then runs the result as an ordinary query, so you can write dbt templates at the `psql` prompt and get normal result tables:

```sql
SELECT dbtindb_sql($$
  select country_name, sum(total_cents) / 100.0 as revenue
  from {{ ref('customers') }}
  group by 1
$$) \gexec
```

The sqlite3 shell has no `\gexec`, so `dbtindb_view(name, sql)` creates (or replaces) a temporary view over the rendered SQL instead. Inside SQLite's single-quoted strings, use Jinja's double quotes:

```sql
SELECT dbtindb_view('q', 'select * from {{ ref("customers") }} where total_cents > 1000');
SELECT * FROM q;
```

`dbtindb_view` is also on PostgreSQL, for clients without `\gexec`. Both functions return the rendered SQL. `dbtindb_view` only ever drops a temporary view of that name, and if the new query fails, the old view is kept.

## PostgreSQL

```sql
dbtindb_build(selector text = NULL, exclude text = NULL, vars jsonb = '{}',
              full_refresh bool = false, target_schema text = 'public')
  RETURNS TABLE(step int, unique_id text, name text, resource_type text,
                action text, status text, failures bigint, sql text)
-- dbtindb_run, dbtindb_seed, dbtindb_snapshot, dbtindb_test: the same

dbtindb_ls(selector text = NULL, exclude text = NULL, vars jsonb = '{}', target_schema text = 'public')
  RETURNS TABLE(build_order int, unique_id text, name text, resource_type text, materialized text,
                relation text, path text, depends_on text[], sources text[], tags text[])

dbtindb_compile(name text, vars jsonb = '{}', target_schema text = 'public') RETURNS text

dbtindb_sql(sql text, vars jsonb = '{}', target_schema text = 'public') RETURNS text
dbtindb_view(name text, sql text, vars jsonb = '{}', target_schema text = 'public') RETURNS text
```

```sql
-- Rebuild the marts and everything they need, with a var.
SELECT * FROM dbtindb_run('+tag:mart', vars => '{"since": "2024-01-01"}', target_schema => 'analytics');

-- Which models read from raw.orders?
SELECT name FROM dbtindb_ls('source:raw.orders+', exclude => 'resource_type:test');

-- Nightly with pg_cron.
SELECT cron.schedule('dbtindb', '0 3 * * *', $$SELECT count(*) FROM dbtindb_build(target_schema => 'analytics')$$);
```

`target_schema` is dbt's `target.schema`. A model with `+schema: marts` is built into `<target_schema>_marts`, as dbt's default `generate_schema_name` does, and missing schemas are created. Identifiers longer than PostgreSQL's 63-byte limit are rejected, because Postgres would silently truncate them. `log()` and `exceptions.warn()` are raised as `NOTICE`s. With `persist_docs`, descriptions become `COMMENT`s.

The project lives in `dbtindb_files`, and the relations dbtindb built are recorded in `dbtindb_managed`. Both are in the extension's schema, and `pg_dump` includes them.

## SQLite

SQLite functions return a single value, so results are JSON that you can query with `json_each()`. Each function takes one optional argument: either a selector (`'+customers'`) or a JSON object `{"select": ..., "exclude": ..., "vars": {...}, "full_refresh": true}`.

| Function | Returns |
|---|---|
| `dbtindb_build([options])`, `dbtindb_run`, `dbtindb_seed`, `dbtindb_snapshot`, `dbtindb_test` | `{"steps": [...], "logs": [...]}`; each step is an object with the columns above, and `logs` holds `log()` and `exceptions.warn()` messages |
| `dbtindb_ls([options])` | A JSON array of nodes |
| `dbtindb_compile(name [, options])` | The compiled SQL |
| `dbtindb_sql(sql [, options])` | Ad-hoc SQL rendered against the project |
| `dbtindb_view(name, sql [, options])` | Creates the temporary view `name` over the rendered SQL, and returns that SQL |

```sql
SELECT json_extract(value, '$.name'), json_extract(value, '$.status')
FROM json_each(dbtindb_build('tag:nightly'), '$.steps');
```

Everything is built in `main`, since a SQLite view can only reference objects in its own schema. `ref()` therefore renders as `"<name>"`, and a custom `schema` config is an error. Merge is not available as an incremental strategy (use delete+insert), and `persist_docs` does nothing, because SQLite has no comments. Seeds store booleans as 1/0.

The functions are `DIRECTONLY`: they can be called from your SQL but not from views or triggers, where anyone who can change the schema could make other users run them. A command runs in a savepoint, so inside your own transaction it commits or rolls back with it.

SQLite only checks a view's columns when the view is queried, so a view model that references a missing column builds fine and fails on first use. Tables fail during the build.

## What is supported from dbt

| | |
|---|---|
| **Materializations** | `view`, `table`, `ephemeral` (inlined as CTEs), `incremental`, `materialized_view` (PostgreSQL), seeds and snapshots. Custom `{% materialization %}` blocks are not supported. |
| **Incremental models** | `is_incremental()`, `{{ this }}`, `unique_key` (one or more columns), `incremental_strategy` `append`, `delete+insert` and `merge` (PostgreSQL 15+), `on_schema_change` `ignore`, `fail`, `append_new_columns` and `sync_all_columns`, and `full_refresh` (per call or per model). |
| **Snapshots** | `{% snapshot %}` blocks and dbt 1.9's YAML snapshots (`relation: ref(...)` or `source(...)`). `strategy='timestamp'` (`updated_at`) and `strategy='check'` (`check_cols`, or `'all'`), `unique_key`, and `hard_deletes` `ignore`, `invalidate` (or `invalidate_hard_deletes`) and `new_record`, which adds a version with `dbt_is_deleted = 'True'`. dbt's `dbt_scd_id`, `dbt_updated_at`, `dbt_valid_from` and `dbt_valid_to` columns. Snapshots go to `target_schema`, or the schema their config gives. |
| **Seeds** | Type inference, `column_types`, `delimiter`. A reload keeps the table when its columns are unchanged. |
| **Tests** | `unique`, `not_null`, `accepted_values` (`quote`), `relationships`, custom generic tests (`{% test %}`, with or without `arguments:`), package tests (`dbt_utils.expression_is_true`), singular tests, and `severity`, `warn_if`, `error_if`, `fail_calc`, `limit`, `where` and `enabled` configs. Tests on sources work too. `store_failures` (or `store_failures_as: table` or `view`) keeps the failing rows in `<target schema>_dbt_test__audit.<test name>` (on SQLite, in `main`). The rows of a failing `dbtindb_test` stay; a failing build rolls them back with everything else. |
| **Unit tests** | dbt 1.8 `unit_tests:` with `given` inputs (`ref`, `source`, and `this` for incremental models) in the `dict`, `csv` and `sql` formats, `expect`, and `overrides` of `vars`, `env_vars` and the `is_incremental` macro. Fixture rows are cast to the column types of the inputs, so those must exist in the database (`dbtindb_build` builds them first), unless the rows are given as `sql`. Rows are compared as multisets over the columns `expect` names, and the differing rows are logged. |
| **Docs** | `{% docs %}` blocks and `doc()` in descriptions, and `persist_docs` as comments on PostgreSQL. |
| **Analyses** | Compiled with `ref`, `source` and macros, never run. |
| **Grants** | `grants` on PostgreSQL: after a build, the privileges others hold on the relation are made equal to the config, merged as dbt does (`+select` adds grantees). SQLite has no roles and ignores them. |
| **Config** | `config()` in files, `config:` in property files, and `+key` in `dbt_project.yml`, merged as dbt does (tags union, hooks append). Keys: `materialized`, `schema`, `alias`, `enabled`, `tags`, `meta`, `pre_hook`, `post_hook`, `persist_docs`, `grants`, `full_refresh`, plus the incremental, snapshot, seed and test keys above. |
| **Hooks** | `pre_hook` and `post_hook` per node, `on-run-start` and `on-run-end` per project. Hooks can use `run_query`, `this` and `target`. |
| **Jinja context** | `ref` (one argument, or two naming this project), `source`, `doc`, `config`, `var`, `env_var`, `this`, `target`, `execute`, `model`, `project_name`, `is_incremental`, `run_query` (agate-like rows and columns), `statement` blocks with `load_result`/`store_result`, `load_relation`, `log`, `print`, `exceptions` (`raise_compiler_error`, `warn`), `adapter` (`dispatch`, `quote`, `execute`, `get_relation`, `load_relation`, `get_columns_in_relation`, `type`), `api.Column` and `api.Relation`, `modules` (`datetime`, `re`, `pytz`), `flags`, `run_started_at`, `invocation_id`, `dbt_version`, `fromjson`, `tojson`, `fromyaml`, `toyaml`, `zip`, `set`, `try_or_compiler_error` and `local_md5`, and Python's string, list and dict methods. Relations and columns are dbt-like objects (`relation.identifier`, `column.quoted`, `column.is_string()`). |
| **Macros** | Global, as in dbt, and they can override `generate_schema_name` and `generate_alias_name`. Packages and dbt's cross-database macros as described in [Packages](#packages), with dbt's `adapter.dispatch` search order and `dispatch:` config. |
| **Not supported** | Package models, seeds and snapshots, cross-project or versioned `ref`, `database` configs, Python models, unit test fixture files and overrides of macros other than `is_incremental`, exposures, metrics, semantic models, `dbt source freshness` and `dbt docs`. Each is an error or ignored, never a silently different build. |

## How a command runs

1. **Parse:** every file is read from `dbtindb_files`, and every model, snapshot and test is rendered with `execute = false`, as dbt's parser does. This collects `ref`s, `source`s and `config`. Disabled nodes are dropped, relations are named with `generate_schema_name`/`generate_alias_name`, and duplicate relations are reported.
2. **Plan:** the selected nodes are ordered topologically. A cycle is reported with its path, for example `dependency cycle: a -> c -> b -> a`. Each node is planned as a replace, an update (incremental, snapshot, seed reload or materialized-view refresh) or a query (tests, and unit tests, which compile their model on fixture rows).
3. **Execute:** schemas are created, then `on-run-start` runs. Replaced relations are dropped dependents-first, and nodes are built in order with their hooks. Models are rendered again here with `execute = true`, so `run_query` and `is_incremental()` see the database. Last comes `on-run-end`.
4. All of this runs in one transaction: the caller's on PostgreSQL, and a savepoint on SQLite. Any error rolls back the whole command.

Rebuilds are deliberately conservative:

- dbtindb only drops relations listed in `dbtindb_managed`. If a table you created stands where a model would go, the command fails instead of replacing it.
- Drops never cascade. A managed view on top of a replaced model is rebuilt with it, but a view of yours makes the rebuild fail, so it is never silently dropped.
- Renders have an instruction budget, so a runaway macro cannot pin a database backend.
- A model cannot call a dbtindb function: a build cannot start another build.

## FAQ

### Is this dbt?

No. dbtindb uses dbt's Jinja engine and follows dbt's project layout, configs, commands and semantics, so projects that stick to [the supported features](#what-is-supported-from-dbt) run unchanged. It does not use dbt's Python or Fusion runtime, adapters or manifest, and it is not affiliated with dbt Labs.

### Why not just run dbt?

You should if you already run dbt, or if you need a warehouse, packages that define models, or the wider dbt ecosystem. dbtindb is for cases where an external process is the problem: projects that ship inside an application database or a SQLite file, builds triggered from SQL or `pg_cron`, and environments where you only have a SQL connection.

### Can I use it on Amazon RDS, Cloud SQL, Supabase or Neon?

Only where you can install custom extensions, which managed services generally do not allow. It works on self-hosted PostgreSQL, in Docker images, and anywhere SQLite can load extensions.

### Who can run a build?

On PostgreSQL, the commands run with the caller's privileges. They need `CREATE` on the target schemas (or on the database, to create them), `SELECT` on the sources and on `dbtindb_files`, and write access to the bookkeeping table:

```sql
GRANT SELECT ON dbtindb_files TO builder;
GRANT SELECT, INSERT, DELETE ON dbtindb_managed TO builder;
```

Anyone who can write to `dbtindb_files` decides what SQL the next build executes, so grant that like DDL rights.

## Development

```sh
# Core: parsing, selection, ordering, compilation
cargo test -p dbtindb-core

# PostgreSQL extension, in a pgrx-managed Postgres
cargo pgrx test pg16 -p pg_dbtindb

# CLI
cargo test -p dbtindb-cli

# SQLite extension and CLI, end to end through Python's sqlite3
cargo build --release -p sqlite_dbtindb -p dbtindb-cli
python3 -m unittest discover -s crates/sqlite_dbtindb/tests -v
```

The workspace has four crates. `dbtindb-core` holds the dbt semantics: the project, the Jinja context, selection, the DAG and the materializations. It drives the database through a small `Database` trait. `pg_dbtindb` (over SPI) and `sqlite_dbtindb` (over the SQLite C API) implement that trait and expose the SQL functions. `dbtindb-cli` is the `dbtindb` command.

## License

MIT. dbt's MiniJinja fork and its contrib crate, dependencies, are licensed under Apache 2.0, as are dbt's macros built into dbtindb (`crates/dbtindb-core/dbt-macros/`, see its `NOTICE`).
