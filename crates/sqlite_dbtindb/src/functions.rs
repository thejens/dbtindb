//! The SQL functions: `dbtindb_compile`, `dbtindb_models` and `dbtindb_run`.
//!
//! SQLite scalar functions return one value, so the list-shaped results are
//! JSON arrays, ready for `json_each()`.

use std::cell::Cell;
use std::ffi::{CStr, c_int};

use dbtindb_core::{
    CompiledModel, Dialect, Error, Existing, ExistingKind, Project, Statement, Target, build_order,
    compile, plan,
};
use libsqlite3_sys as ffi;
use serde_json::{Value, json};

use crate::sqlite::{self, Arg, Call, Db};

/// The table dbtindb reads models from: the same one sqlite-jinja's
/// `jinja_render_named` renders from.
const TEMPLATES_DDL: &str =
    "CREATE TABLE jinja_templates (name TEXT PRIMARY KEY, source TEXT NOT NULL)";

/// Relations created by `dbtindb_run()`. dbtindb only ever drops relations
/// listed here. SQLite resolves identifiers case-insensitively, so lookups
/// do too.
const MANAGED_DDL: &str = "CREATE TABLE IF NOT EXISTS main.dbtindb_managed (
    relation_name TEXT PRIMARY KEY COLLATE NOCASE,
    path          TEXT NOT NULL,
    materialized  TEXT NOT NULL CHECK (materialized IN ('view', 'table')),
    built_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
)";

/// Models are always built in `main`: a SQLite view can only reference
/// objects in its own schema.
const SCHEMA: &str = "main";

thread_local! {
    /// Set while `dbtindb_run` executes model SQL on this thread. A model
    /// that calls `dbtindb_run` itself would rebuild the project from
    /// inside its own build.
    static RUNNING: Cell<bool> = const { Cell::new(false) };
}

/// The Rust side of one SQL function.
type Impl = fn(&Call<'_>) -> Result<String, Error>;

/// Registers the functions on `db`.
///
/// # Safety
/// `db` must be an open connection.
pub unsafe fn register(db: *mut ffi::sqlite3) -> Result<(), String> {
    // All of them read jinja_templates and so are not deterministic.
    // DIRECTONLY keeps them out of views, triggers and generated columns,
    // where whoever can write the schema could make other users run them.
    let flags = ffi::SQLITE_UTF8 | ffi::SQLITE_DIRECTONLY;
    let functions: [(&CStr, c_int, Impl); 6] = [
        (c"dbtindb_compile", 1, dbtindb_compile),
        (c"dbtindb_compile", 2, dbtindb_compile),
        (c"dbtindb_models", 0, dbtindb_models),
        (c"dbtindb_models", 1, dbtindb_models),
        (c"dbtindb_run", 0, dbtindb_run),
        (c"dbtindb_run", 1, dbtindb_run),
    ];
    for (name, n_arg, f) in functions {
        // SAFETY: forwarded from the caller.
        unsafe {
            sqlite::create_function(
                db,
                name,
                n_arg,
                flags,
                Box::new(move |call| f(call).map_err(|e| format!("dbtindb: {e}"))),
            )?;
        }
    }
    Ok(())
}

/// `dbtindb_compile(model [, vars])`: the model compiled to plain SQL.
fn dbtindb_compile(call: &Call<'_>) -> Result<String, Error> {
    let model = match call.arg(0) {
        Arg::Text(t) => utf8(t)?,
        other => {
            return Err(Error::new(format!(
                "model must be text, not {}",
                type_name(other)
            )));
        }
    };
    let models = compile_project(call.db(), &vars(call.arg(1))?)?;
    models
        .into_iter()
        .find(|m| m.name == model)
        .map(|m| m.sql)
        .ok_or_else(|| Error::new(format!("no enabled model named {model:?}")))
}

/// `dbtindb_models([vars])`: the enabled models in build order, with their
/// lineage, as a JSON array.
fn dbtindb_models(call: &Call<'_>) -> Result<String, Error> {
    let models = compile_project(call.db(), &vars(call.arg(0))?)?;
    let order = build_order(&models)?;
    let rows: Vec<Value> = order
        .iter()
        .enumerate()
        .map(|(pos, &i)| {
            let m = &models[i];
            json!({
                "build_order": pos + 1,
                "model": m.name,
                "path": m.path,
                "materialized": m.materialized.as_str(),
                "relation": m.relation,
                "depends_on": m.depends_on,
                "sources": m.sources,
            })
        })
        .collect();
    Ok(Value::Array(rows).to_string())
}

/// `dbtindb_run([vars])`: builds every enabled model, in dependency order,
/// and returns the statements it ran as a JSON array.
///
/// The build runs inside a savepoint: if any statement fails, everything it
/// did is rolled back, and inside a caller's transaction it commits or rolls
/// back with that transaction.
fn dbtindb_run(call: &Call<'_>) -> Result<String, Error> {
    if RUNNING.get() {
        return Err(Error::new(
            "dbtindb_run() cannot be called while it is running",
        ));
    }
    let vars = vars(call.arg(0))?;
    let db = call.db();
    let _running = Running::enter();
    db.exec("SAVEPOINT dbtindb_run").map_err(Error::new)?;
    let built = build(db, &vars);
    let end = match &built {
        Ok(_) => db.exec("RELEASE dbtindb_run"),
        Err(_) => db
            .exec("ROLLBACK TO dbtindb_run")
            .and_then(|()| db.exec("RELEASE dbtindb_run")),
    };
    // The build's own error matters more than a failure to end the
    // savepoint after it.
    let steps = built?;
    end.map_err(Error::new)?;
    Ok(Value::Array(steps).to_string())
}

/// Holds [`RUNNING`] for as long as it lives, including through a panic.
struct Running;

impl Running {
    fn enter() -> Self {
        RUNNING.set(true);
        Running
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.set(false);
    }
}

fn build(db: Db, vars: &Value) -> Result<Vec<Value>, Error> {
    db.exec(MANAGED_DDL).map_err(Error::new)?;
    let models = compile_project(db, vars)?;
    let order = build_order(&models)?;
    let statements = plan(&models, &order, |m| existing(db, &m.name))?;

    let mut steps = Vec::with_capacity(statements.len());
    for (step, statement) in statements.iter().enumerate() {
        db.exec(statement.sql())
            .map_err(|e| Error::new(format!("{e}\nwhile running:\n{}", statement.sql())))?;
        // The bookkeeping row moves in the same savepoint as the DDL, so
        // dbtindb_managed always matches what actually exists.
        let (model, action) = match statement {
            Statement::Drop { model, .. } => {
                db.query(
                    "DELETE FROM main.dbtindb_managed WHERE relation_name = ?1",
                    &[model],
                )
                .map_err(Error::new)?;
                (model, "drop")
            }
            Statement::Create { model, .. } => {
                let compiled = find(&models, model);
                db.query(
                    "INSERT INTO main.dbtindb_managed (relation_name, path, materialized) \
                     VALUES (?1, ?2, ?3)",
                    &[model, &compiled.path, compiled.materialized.as_str()],
                )
                .map_err(Error::new)?;
                (model, "create")
            }
        };
        steps.push(json!({
            "step": step + 1,
            "model": model,
            "action": action,
            "statement": statement.sql(),
        }));
    }
    Ok(steps)
}

/// What exists in `main` under a model's name, and whether dbtindb made it.
fn existing(db: Db, name: &str) -> Result<Option<Existing>, Error> {
    let rows = db
        .query(
            "SELECT type FROM main.sqlite_master WHERE name = ?1 COLLATE NOCASE",
            &[name],
        )
        .map_err(Error::new)?;
    let Some(kind) = rows
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next().flatten())
    else {
        return Ok(None);
    };
    let kind = match kind.as_str() {
        "view" => ExistingKind::View,
        "table" => ExistingKind::Table,
        other => ExistingKind::Other(other.to_owned()),
    };
    let managed = !db
        .query(
            "SELECT 1 FROM main.dbtindb_managed WHERE relation_name = ?1",
            &[name],
        )
        .map_err(Error::new)?
        .is_empty();
    Ok(Some(Existing { kind, managed }))
}

fn compile_project(db: Db, vars: &Value) -> Result<Vec<CompiledModel>, Error> {
    let exists = !db
        .query(
            "SELECT 1 FROM main.sqlite_master WHERE type = 'table' AND name = 'jinja_templates'",
            &[],
        )
        .map_err(Error::new)?
        .is_empty();
    if !exists {
        return Err(Error::new(format!(
            "no jinja_templates table to read models from (create it with: {TEMPLATES_DDL})"
        )));
    }
    let files = db
        .query("SELECT name, source FROM main.jinja_templates", &[])
        .map_err(Error::new)?
        .into_iter()
        .filter_map(|row| {
            let mut cols = row.into_iter();
            Some((cols.next()??, cols.next()??))
        });
    let project = Project::new(files)?;
    let target = Target {
        dialect: Dialect::Sqlite,
        schema: SCHEMA.to_owned(),
    };
    compile(&project, &target, vars)
}

/// The optional `vars` argument: a JSON object, or NULL for none.
fn vars(arg: Arg<'_>) -> Result<Value, Error> {
    match arg {
        Arg::Null => Ok(json!({})),
        Arg::Text(t) => match serde_json::from_slice(t) {
            Ok(v @ Value::Object(_)) => Ok(v),
            Ok(_) => Err(Error::new("vars must be a JSON object")),
            Err(e) => Err(Error::new(format!("vars is not valid JSON: {e}"))),
        },
        other => Err(Error::new(format!(
            "vars must be a JSON object, not {}",
            type_name(other)
        ))),
    }
}

fn utf8(t: &[u8]) -> Result<&str, Error> {
    std::str::from_utf8(t).map_err(|_| Error::new("argument is not valid UTF-8"))
}

fn type_name(arg: Arg<'_>) -> &'static str {
    match arg {
        Arg::Null => "null",
        Arg::Text(_) => "text",
        Arg::Other(name) => name,
    }
}

fn find<'a>(models: &'a [CompiledModel], name: &str) -> &'a CompiledModel {
    models
        .iter()
        .find(|m| m.name == name)
        .expect("planned statements only name compiled models")
}
