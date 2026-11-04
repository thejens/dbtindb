//! The SQL-callable functions.
//!
//! Everything runs in the caller's transaction: a failure anywhere (an
//! error-severity test in `dbtindb_build` included) raises an error and
//! rolls back everything the call did.

use std::cell::Cell;
use std::sync::Arc;

use dbtindb_core::{Command, Dialect, Error, Options, Session, Step, Target};
use pgrx::prelude::*;
use pgrx::JsonB;

use crate::database::{extension_table, PgDb};

thread_local! {
    /// Set while a command runs. Model SQL that called a dbtindb function
    /// would rebuild the project from inside its own build.
    static RUNNING: Cell<bool> = const { Cell::new(false) };
}

/// Defines one command function. pgrx reads the result columns from the
/// signature, so each function spells them out.
macro_rules! command_fn {
    ($(#[$doc:meta])* $name:ident, $command:expr) => {
        $(#[$doc])*
        #[allow(clippy::type_complexity)]
        #[pg_extern(volatile, parallel_unsafe)]
        fn $name(
            selector: default!(Option<&str>, "NULL"),
            exclude: default!(Option<&str>, "NULL"),
            vars: default!(JsonB, "'{}'::jsonb"),
            full_refresh: default!(bool, false),
            target_schema: default!(&str, "'public'"),
        ) -> TableIterator<
            'static,
            (
                name!(step, i32),
                name!(unique_id, String),
                name!(name, String),
                name!(resource_type, String),
                name!(action, String),
                name!(status, String),
                name!(failures, Option<i64>),
                name!(sql, String),
            ),
        > {
            let options = Options {
                select: selector.unwrap_or_default().to_owned(),
                exclude: exclude.unwrap_or_default().to_owned(),
                full_refresh,
            };
            let steps = invoke($command, &options, &vars.0, target_schema).unwrap_or_else(|e| error!("dbtindb: {e}"));
            TableIterator::new(steps.into_iter().map(|s| {
                (
                    i32::try_from(s.step).expect("fewer than 2^31 steps"),
                    s.unique_id,
                    s.name,
                    s.resource_type,
                    s.action,
                    s.status,
                    s.failures,
                    s.sql,
                )
            }))
        }
    };
}

command_fn!(
    /// `dbt build`: seeds, models, snapshots and tests in DAG order, each
    /// node after the tests of what it reads. A failing error-severity test
    /// aborts the build.
    dbtindb_build,
    Command::Build
);
command_fn!(
    /// `dbt run`: builds the selected models.
    dbtindb_run,
    Command::Run
);
command_fn!(
    /// `dbt seed`: loads the selected seeds.
    dbtindb_seed,
    Command::Seed
);
command_fn!(
    /// `dbt snapshot`: runs the selected snapshots.
    dbtindb_snapshot,
    Command::Snapshot
);
command_fn!(
    /// `dbt test`: runs the selected tests and reports failures as rows
    /// with status `fail` instead of raising.
    dbtindb_test,
    Command::Test
);

fn invoke(
    command: Command,
    options: &Options,
    vars: &serde_json::Value,
    schema: &str,
) -> Result<Vec<Step>, Error> {
    let _running = Running::enter()?;
    let session = load(vars, schema)?;
    dbtindb_core::invoke(&session, command, options)
}

/// `dbt ls`: the selected nodes in build order, with their lineage.
#[allow(clippy::type_complexity)]
#[pg_extern(volatile, parallel_unsafe)]
fn dbtindb_ls(
    selector: default!(Option<&str>, "NULL"),
    exclude: default!(Option<&str>, "NULL"),
    vars: default!(JsonB, "'{}'::jsonb"),
    target_schema: default!(&str, "'public'"),
) -> TableIterator<
    'static,
    (
        name!(build_order, i32),
        name!(unique_id, String),
        name!(name, String),
        name!(resource_type, String),
        name!(materialized, Option<String>),
        name!(relation, Option<String>),
        name!(path, String),
        name!(depends_on, Vec<String>),
        name!(sources, Vec<String>),
        name!(tags, Vec<String>),
    ),
> {
    let options = Options {
        select: selector.unwrap_or_default().to_owned(),
        exclude: exclude.unwrap_or_default().to_owned(),
        full_refresh: false,
    };
    let listed = load(&vars.0, target_schema)
        .and_then(|session| dbtindb_core::ls(&session, &options))
        .unwrap_or_else(|e| error!("dbtindb: {e}"));
    TableIterator::new(listed.into_iter().map(|n| {
        (
            i32::try_from(n.build_order).expect("fewer than 2^31 nodes"),
            n.unique_id,
            n.name,
            n.resource_type,
            n.materialized,
            n.relation,
            n.path,
            n.depends_on,
            n.sources,
            n.tags,
        )
    }))
}

/// `dbt compile`: the SQL a model, snapshot or test runs. Volatile because
/// compiling evaluates the Jinja, which may call `run_query()`.
#[pg_extern(volatile, parallel_unsafe)]
fn dbtindb_compile(
    name: &str,
    vars: default!(JsonB, "'{}'::jsonb"),
    target_schema: default!(&str, "'public'"),
) -> String {
    load(&vars.0, target_schema)
        .and_then(|session| dbtindb_core::compile(&session, name))
        .unwrap_or_else(|e| error!("dbtindb: {e}"))
}

/// Loads the project from `dbtindb_files`.
fn load(vars: &serde_json::Value, schema: &str) -> Result<Session, Error> {
    let db = Arc::new(PgDb::new()?);
    let files = Spi::connect(|client| -> Result<Vec<(String, String)>, Error> {
        let table = extension_table(client, "pg_dbtindb", "dbtindb_files")?;
        let spi = |e| Error::new(format!("SPI error: {e}"));
        let mut files = Vec::new();
        for row in client
            .select(&format!("SELECT path, source FROM {table}"), None, &[])
            .map_err(spi)?
        {
            let path: Option<String> = row.get(1).map_err(spi)?;
            let source: Option<String> = row.get(2).map_err(spi)?;
            if let (Some(path), Some(source)) = (path, source) {
                files.push((path, source));
            }
        }
        Ok(files)
    })?;
    let target = Target {
        dialect: Dialect::Postgres,
        schema: schema.to_owned(),
    };
    dbtindb_core::load(files, &target, vars, db)
}

/// Holds [`RUNNING`] for as long as it lives, including while a Postgres
/// error unwinds through it.
struct Running;

impl Running {
    fn enter() -> Result<Self, Error> {
        if RUNNING.replace(true) {
            return Err(Error::new(
                "a dbtindb command cannot run while another is running",
            ));
        }
        Ok(Running)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.set(false);
    }
}
