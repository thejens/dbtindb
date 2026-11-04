//! dbinja for PostgreSQL: dbt-style Jinja SQL models, built inside the
//! database from pgjinja's `jinja_templates` table.

use pgrx::prelude::*;

mod catalog;
mod functions;
#[cfg(any(test, feature = "pg_test"))]
mod pg_tests;

::pgrx::pg_module_magic!(name, version);

extension_sql!(
    r#"
CREATE TABLE dbinja_managed (
    schema_name   text NOT NULL,
    relation_name text NOT NULL,
    path          text NOT NULL,
    materialized  text NOT NULL CHECK (materialized IN ('view', 'table')),
    built_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (schema_name, relation_name)
);
COMMENT ON TABLE dbinja_managed IS
    'Relations created by dbinja_run(). dbinja only ever drops relations listed here.';
SELECT pg_catalog.pg_extension_config_dump('dbinja_managed', '');
"#,
    name = "dbinja_managed_table",
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
