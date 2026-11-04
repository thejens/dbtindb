//! dbtindb for PostgreSQL: a dbt project (models, seeds, snapshots, tests,
//! macros) built inside the database from its `dbtindb_files` table.

use pgrx::prelude::*;

mod database;
mod functions;
#[cfg(any(test, feature = "pg_test"))]
mod pg_tests;

::pgrx::pg_module_magic!(name, version);

extension_sql!(
    r#"
CREATE TABLE dbtindb_files (
    path   text PRIMARY KEY,
    source text NOT NULL
);
COMMENT ON TABLE dbtindb_files IS
    'The dbt project: one row per file (dbt_project.yml, models/*.sql, seeds/*.csv, ...), keyed by its path in the project.';
SELECT pg_catalog.pg_extension_config_dump('dbtindb_files', '');

CREATE TABLE dbtindb_managed (
    schema_name   text NOT NULL,
    relation_name text NOT NULL,
    unique_id     text NOT NULL,
    materialized  text NOT NULL,
    -- For materialized views: the compiled query's MD5, to refresh rather
    -- than rebuild while it is unchanged.
    checksum      text,
    built_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (schema_name, relation_name)
);
COMMENT ON TABLE dbtindb_managed IS
    'Relations built by dbtindb. dbtindb only ever drops or replaces relations listed here.';
SELECT pg_catalog.pg_extension_config_dump('dbtindb_managed', '');
"#,
    name = "dbtindb_managed_table",
);

/// Required by `cargo pgrx test`; must be visible at the crate root.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
