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
    let functions: [(&CStr, &[c_int], Impl); 8] = [
        (c"dbtindb_build", &[0, 1], |c| command(c, Command::Build)),
        (c"dbtindb_run", &[0, 1], |c| command(c, Command::Run)),
        (c"dbtindb_seed", &[0, 1], |c| command(c, Command::Seed)),
        (c"dbtindb_snapshot", &[0, 1], |c| {
            command(c, Command::Snapshot)
        }),
        (c"dbtindb_test", &[0, 1], |c| command(c, Command::Test)),
        (c"dbtindb_ls", &[0, 1], ls),
        (c"dbtindb_compile", &[1, 2], compile),
        (c"dbtindb_md5", &[1], md5),
    ];
    for (name, arities, f) in functions {
        for &n in arities {
            let pure = name == c"dbtindb_md5";
            let flags = if pure {
                ffi::SQLITE_UTF8 | ffi::SQLITE_DETERMINISTIC
            } else {
                flags
            };
            // SAFETY: forwarded from the caller.
            unsafe {
                sqlite::create_function(
                    db,
                    name,
                    n,
                    flags,
                    Box::new(move |call| f(call).map_err(|e| format!("dbtindb: {e}"))),
                )?;
            }
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

fn md5(call: &Call<'_>) -> Result<String, Error> {
    let bytes = match call.arg(0) {
        Arg::Text(t) => t,
        _ => &[],
    };
    Ok(Md5::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
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
