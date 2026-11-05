//! The SQL functions. DuckDB scalar functions return one value, so list
//! results are JSON.

use std::sync::Arc;

use dbtindb_core::{Command, Dialect, Error, Invocation, Session, Target};
use serde_json::json;

use crate::database::{DuckDb, MANAGED_DDL};
use crate::duckdb::{Args, Connection, Function, Type};

/// The table dbtindb reads the project from: one row per project file.
const FILES_DDL: &str =
    "CREATE TABLE dbtindb_files (path VARCHAR PRIMARY KEY, source VARCHAR NOT NULL)";

/// Options are a selection string (`'orders+'`) or JSON: a JSON string, or
/// a STRUCT such as `{'select': 'orders+', 'vars': {'day': '2024-01-01'}}`,
/// which DuckDB casts to JSON. Both overloads exist so that plain strings are
/// not parsed as JSON by DuckDB's cast.
const OPTIONS: &[&[Type]] = &[&[], &[Type::Varchar], &[Type::Json]];
const WITH_OPTIONS: &[&[Type]] = &[
    &[Type::Varchar],
    &[Type::Varchar, Type::Varchar],
    &[Type::Varchar, Type::Json],
];

type Impl = fn(&Arc<Connection>, &Args<'_>) -> Result<String, Error>;

/// Registers the functions through `conn`, which also runs everything they
/// do.
pub fn register(conn: Arc<Connection>) -> Result<(), String> {
    use Type::{Json, Varchar};
    let functions: [(&str, &[&[Type]], Type, Impl); 8] = [
        ("dbtindb_build", OPTIONS, Json, |c, a| {
            command(c, a, Command::Build)
        }),
        ("dbtindb_run", OPTIONS, Json, |c, a| {
            command(c, a, Command::Run)
        }),
        ("dbtindb_seed", OPTIONS, Json, |c, a| {
            command(c, a, Command::Seed)
        }),
        ("dbtindb_snapshot", OPTIONS, Json, |c, a| {
            command(c, a, Command::Snapshot)
        }),
        ("dbtindb_test", OPTIONS, Json, |c, a| {
            command(c, a, Command::Test)
        }),
        ("dbtindb_ls", OPTIONS, Json, ls),
        ("dbtindb_compile", WITH_OPTIONS, Varchar, compile),
        ("dbtindb_sql", WITH_OPTIONS, Varchar, sql),
    ];
    for (name, overloads, ret, f) in functions {
        let c = conn.clone();
        let call = Arc::new(move |args: &Args<'_>| {
            let _claim = c.claim().map_err(|e| format!("dbtindb: {e}"))?;
            f(&c, args).map(Some).map_err(|e| format!("dbtindb: {e}"))
        });
        let mut function = Function::new(name);
        for params in overloads {
            function = function.overload(params, ret, call.clone());
        }
        conn.register(function)?;
    }
    Ok(())
}

/// Builds (part of) the project in one transaction of the extension's
/// connection: if anything fails, everything it did is rolled back. It
/// commits on its own, whatever the caller's transaction does after.
fn command(conn: &Arc<Connection>, args: &Args<'_>, cmd: Command) -> Result<String, Error> {
    let inv = Invocation::parse(text_arg(args, 0, "options")?)?;
    let run = |sql: &str| conn.query(sql, &[]).map(drop).map_err(Error::new);
    run("BEGIN TRANSACTION")?;
    let result = (|| -> Result<String, Error> {
        run(MANAGED_DDL)?;
        let (session, db) = load(conn, &inv)?;
        let steps = dbtindb_core::invoke(&session, cmd, &inv.options)?;
        let logs = std::mem::take(&mut *db.logs.lock().unwrap());
        Ok(json!({"steps": dbtindb_core::steps_json(&steps), "logs": logs}).to_string())
    })();
    let end = run(if result.is_ok() { "COMMIT" } else { "ROLLBACK" });
    // The command's own error matters more than a failure to end the
    // transaction after it.
    let out = result?;
    end?;
    Ok(out)
}

/// `dbtindb_ls([options])`: the selected nodes in build order, with lineage.
fn ls(conn: &Arc<Connection>, args: &Args<'_>) -> Result<String, Error> {
    let inv = Invocation::parse(text_arg(args, 0, "options")?)?;
    let (session, _) = load(conn, &inv)?;
    let nodes = dbtindb_core::ls(&session, &inv.options)?;
    Ok(serde_json::Value::Array(nodes.iter().map(|n| n.to_json()).collect()).to_string())
}

/// `dbtindb_compile(name [, options])`: a model's, snapshot's or test's SQL.
fn compile(conn: &Arc<Connection>, args: &Args<'_>) -> Result<String, Error> {
    let name =
        text_arg(args, 0, "name")?.ok_or_else(|| Error::new("dbtindb_compile needs a name"))?;
    let inv = Invocation::parse(text_arg(args, 1, "options")?)?;
    let (session, _) = load(conn, &inv)?;
    dbtindb_core::compile(&session, name)
}

/// `dbtindb_sql(sql [, options])`: ad-hoc SQL with `ref`, `source`, macros
/// and vars, rendered against the project, like `dbt compile --inline`.
fn sql(conn: &Arc<Connection>, args: &Args<'_>) -> Result<String, Error> {
    let sql = text_arg(args, 0, "sql")?.ok_or_else(|| Error::new("dbtindb_sql needs SQL"))?;
    let inv = Invocation::parse(text_arg(args, 1, "options")?)?;
    let (session, _) = load(conn, &inv)?;
    dbtindb_core::compile_inline(&session, sql)
}

/// Reads the project from `dbtindb_files` (as committed: the extension's
/// connection does not see the caller's open transaction).
fn load(conn: &Arc<Connection>, inv: &Invocation) -> Result<(Session, Arc<DuckDb>), Error> {
    let db = Arc::new(DuckDb::new(conn.clone())?);
    let files = db
        .query("SELECT path, source FROM main.dbtindb_files", &[])
        .map_err(|e| {
            if e.to_string().contains("dbtindb_files does not exist") {
                Error::new(format!(
                    "no dbtindb_files table to read the project from (create it with: {FILES_DDL})"
                ))
            } else {
                e
            }
        })?
        .rows
        .into_iter()
        .filter_map(|r| Some((r[0].as_str()?.to_owned(), r[1].as_str()?.to_owned())))
        .collect();
    let target = Target {
        dialect: Dialect::DuckDB,
        schema: "main".into(),
    };
    let session = dbtindb_core::load(files, &target, &inv.vars, db.clone())?;
    Ok((session, db))
}

fn text_arg<'a>(args: &Args<'a>, i: usize, what: &str) -> Result<Option<&'a str>, Error> {
    args.get(i)
        .map(|t| {
            std::str::from_utf8(t).map_err(|_| Error::new(format!("{what} is not valid UTF-8")))
        })
        .transpose()
}
