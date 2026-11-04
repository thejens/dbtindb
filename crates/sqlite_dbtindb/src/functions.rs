//! The SQL functions. SQLite scalar functions return one value, so list
//! results are JSON, ready for `json_each()`.

use std::cell::Cell;
use std::ffi::{CStr, c_int};
use std::sync::Arc;

use dbtindb_core::{Command, Dialect, Error, Invocation, Target};
use libsqlite3_sys as ffi;
use md5::{Digest, Md5};
use serde_json::json;

use crate::database::{MANAGED_DDL, SqliteDb};
use crate::sqlite::{self, Arg, Call, Db};

/// The table dbtindb reads the project from: one row per project file.
const FILES_DDL: &str = "CREATE TABLE dbtindb_files (path TEXT PRIMARY KEY, source TEXT NOT NULL)";

thread_local! {
    /// Set while a command runs on this thread. Model SQL that called a
    /// dbtindb function would rebuild the project from inside its own build.
    static RUNNING: Cell<bool> = const { Cell::new(false) };
}

type Impl = fn(&Call<'_>) -> Result<String, Error>;

/// A pure function: NULL in, NULL out, as in PostgreSQL.
type PureFn = fn(&Call<'_>) -> Result<Option<String>, String>;

/// Registers the functions on `db`.
///
/// # Safety
/// `db` must be an open connection.
pub unsafe fn register(db: *mut ffi::sqlite3) -> Result<(), String> {
    // They read dbtindb_files and the schema, so they are not
    // deterministic. DIRECTONLY keeps them out of views, triggers and
    // generated columns, where whoever can write the schema could make
    // other users run them.
    let flags = ffi::SQLITE_UTF8 | ffi::SQLITE_DIRECTONLY;
    let functions: [(&CStr, &[c_int], Impl); 9] = [
        (c"dbtindb_build", &[0, 1], |c| command(c, Command::Build)),
        (c"dbtindb_run", &[0, 1], |c| command(c, Command::Run)),
        (c"dbtindb_seed", &[0, 1], |c| command(c, Command::Seed)),
        (c"dbtindb_snapshot", &[0, 1], |c| {
            command(c, Command::Snapshot)
        }),
        (c"dbtindb_test", &[0, 1], |c| command(c, Command::Test)),
        (c"dbtindb_ls", &[0, 1], ls),
        (c"dbtindb_compile", &[1, 2], compile),
        (c"dbtindb_sql", &[1, 2], sql),
        (c"dbtindb_view", &[2, 3], view),
    ];
    for (name, arities, f) in functions {
        for &n in arities {
            // SAFETY: forwarded from the caller.
            unsafe {
                sqlite::create_function(
                    db,
                    name,
                    n,
                    flags,
                    Box::new(move |call| f(call).map(Some).map_err(|e| format!("dbtindb: {e}"))),
                )?;
            }
        }
    }
    // Pure functions that compiled SQL calls: dbt's `hash` and `split_part`
    // macros on SQLite. They are safe anywhere, including in views.
    let pure: [(&CStr, c_int, PureFn); 2] = [
        (c"dbtindb_md5", 1, md5),
        (c"dbtindb_split_part", 3, split_part),
    ];
    for (name, n, f) in pure {
        // SAFETY: forwarded from the caller.
        unsafe {
            sqlite::create_function(
                db,
                name,
                n,
                ffi::SQLITE_UTF8 | ffi::SQLITE_DETERMINISTIC,
                Box::new(move |call| {
                    f(call).map_err(|e| format!("{}: {e}", name.to_string_lossy()))
                }),
            )?;
        }
    }
    Ok(())
}

/// Builds (part of) the project inside a savepoint: if anything fails,
/// everything it did is rolled back, and inside a caller's transaction it
/// commits or rolls back with that transaction.
fn command(call: &Call<'_>, cmd: Command) -> Result<String, Error> {
    let inv = Invocation::parse(text_arg(call, 0, "options")?)?;
    let raw = call.db();
    let _running = Running::enter()?;
    raw.exec("SAVEPOINT dbtindb").map_err(Error::new)?;
    let result = (|| -> Result<String, Error> {
        raw.exec(MANAGED_DDL).map_err(Error::new)?;
        let (session, db) = load(raw, &inv)?;
        let steps = dbtindb_core::invoke(&session, cmd, &inv.options)?;
        let logs = std::mem::take(&mut *db.logs.lock().unwrap());
        Ok(json!({"steps": dbtindb_core::steps_json(&steps), "logs": logs}).to_string())
    })();
    let end = match &result {
        Ok(_) => raw.exec("RELEASE dbtindb"),
        Err(_) => raw
            .exec("ROLLBACK TO dbtindb")
            .and_then(|()| raw.exec("RELEASE dbtindb")),
    };
    // The command's own error matters more than a failure to end the
    // savepoint after it.
    let out = result?;
    end.map_err(Error::new)?;
    Ok(out)
}

/// `dbtindb_ls([options])`: the selected nodes in build order, with lineage.
fn ls(call: &Call<'_>) -> Result<String, Error> {
    let inv = Invocation::parse(text_arg(call, 0, "options")?)?;
    let (session, _) = load(call.db(), &inv)?;
    let nodes = dbtindb_core::ls(&session, &inv.options)?;
    Ok(serde_json::Value::Array(nodes.iter().map(|n| n.to_json()).collect()).to_string())
}

/// `dbtindb_compile(name [, options])`: a model's, snapshot's or test's SQL.
fn compile(call: &Call<'_>) -> Result<String, Error> {
    let name =
        text_arg(call, 0, "name")?.ok_or_else(|| Error::new("dbtindb_compile needs a name"))?;
    let inv = Invocation::parse(text_arg(call, 1, "options")?)?;
    let (session, _) = load(call.db(), &inv)?;
    dbtindb_core::compile(&session, name)
}

/// `dbtindb_sql(sql [, options])`: ad-hoc SQL with `ref`, `source`, macros
/// and vars, rendered against the project, like `dbt compile --inline`.
fn sql(call: &Call<'_>) -> Result<String, Error> {
    let sql = text_arg(call, 0, "sql")?.ok_or_else(|| Error::new("dbtindb_sql needs SQL"))?;
    let inv = Invocation::parse(text_arg(call, 1, "options")?)?;
    let (session, _) = load(call.db(), &inv)?;
    dbtindb_core::compile_inline(&session, sql)
}

/// `dbtindb_view(name, sql [, options])`: creates (or replaces) the
/// temporary view `name` over SQL rendered like `dbtindb_sql`, which is
/// how the sqlite3 shell can query a dbt template. Returns the SQL.
fn view(call: &Call<'_>) -> Result<String, Error> {
    let name =
        text_arg(call, 0, "name")?.ok_or_else(|| Error::new("dbtindb_view needs a view name"))?;
    let sql = text_arg(call, 1, "sql")?.ok_or_else(|| Error::new("dbtindb_view needs SQL"))?;
    let inv = Invocation::parse(text_arg(call, 2, "options")?)?;
    let raw = call.db();
    let (session, _) = load(raw, &inv)?;
    let rendered = dbtindb_core::compile_inline(&session, sql)?;
    let view = Dialect::Sqlite.quote(name);
    // In a savepoint, so a query that fails to compile keeps the old view;
    // qualified with temp, so only ever a temporary view is replaced.
    raw.exec("SAVEPOINT dbtindb_view").map_err(Error::new)?;
    let created = raw.exec(&format!(
        "DROP VIEW IF EXISTS temp.{view};\nCREATE TEMP VIEW {view} AS\n{rendered}"
    ));
    let end = match &created {
        Ok(()) => raw.exec("RELEASE dbtindb_view"),
        Err(_) => raw
            .exec("ROLLBACK TO dbtindb_view")
            .and_then(|()| raw.exec("RELEASE dbtindb_view")),
    };
    created.map_err(Error::new)?;
    end.map_err(Error::new)?;
    Ok(rendered)
}

/// md5 of the argument's text, as hex.
fn md5(call: &Call<'_>) -> Result<Option<String>, String> {
    Ok(call.text(0).map(|bytes| {
        Md5::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }))
}

/// PostgreSQL's `split_part(string, delimiter, n)`: the n-th field, counting
/// from 1, or from the end when n is negative; '' past the last field.
fn split_part(call: &Call<'_>) -> Result<Option<String>, String> {
    let (Some(text), Some(delim), Some(n)) = (call.text(0), call.text(1), call.int(2)) else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(text);
    let delim = String::from_utf8_lossy(delim);
    if n == 0 {
        return Err("field position must not be zero".into());
    }
    // An empty delimiter does not split: the string is the only field.
    let fields: Vec<&str> = if delim.is_empty() {
        vec![&text]
    } else {
        text.split(delim.as_ref()).collect()
    };
    let index = if n > 0 {
        usize::try_from(n - 1).ok()
    } else {
        usize::try_from(-n)
            .ok()
            .and_then(|back| fields.len().checked_sub(back))
    };
    Ok(Some(
        index
            .and_then(|i| fields.get(i))
            .map_or_else(String::new, |f| (*f).to_owned()),
    ))
}

fn load(raw: Db, inv: &Invocation) -> Result<(dbtindb_core::Session, Arc<SqliteDb>), Error> {
    let db = Arc::new(SqliteDb::new(raw));
    let (_, exists) = raw
        .query(
            "SELECT 1 FROM main.sqlite_master WHERE type = 'table' AND name = 'dbtindb_files'",
            &[],
        )
        .map_err(Error::new)?;
    if exists.is_empty() {
        return Err(Error::new(format!(
            "no dbtindb_files table to read the project from (create it with: {FILES_DDL})"
        )));
    }
    let (_, rows) = raw
        .query("SELECT path, source FROM main.dbtindb_files", &[])
        .map_err(Error::new)?;
    let files = rows
        .into_iter()
        .filter_map(|r| Some((r[0].as_str()?.to_owned(), r[1].as_str()?.to_owned())))
        .collect();
    let target = Target {
        dialect: Dialect::Sqlite,
        schema: "main".into(),
    };
    let session = dbtindb_core::load(files, &target, &inv.vars, db.clone())?;
    Ok((session, db))
}

fn text_arg<'a>(call: &Call<'a>, i: usize, what: &str) -> Result<Option<&'a str>, Error> {
    match call.arg(i) {
        Arg::Null => Ok(None),
        Arg::Text(t) => std::str::from_utf8(t)
            .map(Some)
            .map_err(|_| Error::new(format!("{what} is not valid UTF-8"))),
        Arg::Other(kind) => Err(Error::new(format!("{what} must be text, not {kind}"))),
    }
}

/// Holds [`RUNNING`] for as long as it lives, including through a panic.
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
