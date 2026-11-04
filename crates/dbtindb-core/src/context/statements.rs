//! Running SQL from Jinja the way dbt's `statement` blocks do:
//! `adapter.execute()` runs it, `store_result()` keeps the result under a
//! name for the rest of the render and `load_result()` reads it back.
//!
//! `statement` is native rather than dbt's Jinja macro: a `{% call %}`
//! body must run in the template state that defined it, and a macro in
//! another template would run it in its own, where macros the body calls
//! from its file are out of reach.

use minijinja::{Error, State, Value};

use super::objects::Args;
use super::table::TableObject;
use super::{Shared, jinja_error, to_jinja};
use crate::Table;

/// `adapter.execute(sql, auto_begin=False, fetch=False)`: an adapter
/// response and the rows, if fetched. dbtindb runs inside the caller's
/// transaction, so `auto_begin` changes nothing.
pub fn execute(s: &Shared, args: &Args<'_>) -> Result<Value, Error> {
    let sql = args.string(0, "sql")?;
    let fetch = args.get(2, "fetch").is_some_and(|v| v.is_true());
    let (response, table) = run(s, &sql, fetch)?;
    Ok(Value::from(vec![response, table]))
}

/// `{% call statement(name=None, fetch_result=False, auto_begin=True,
/// language='sql') %}`: runs the block's SQL while executing and keeps the
/// result as `name`.
pub fn statement(s: &Shared, state: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    if !s.current.lock().unwrap().execute {
        return Ok(Value::from(""));
    }
    let language = a.get(3, "language").map(|v| v.to_string());
    if language.as_deref().is_some_and(|l| l != "sql") {
        return Err(jinja_error("statement(): dbtindb only runs SQL statements"));
    }
    let sql = caller_output(state, a)?;
    let fetch = a.get(1, "fetch_result").is_some_and(|v| v.is_true());
    let (response, table) = run(s, &sql, fetch)?;
    if let Some(name) = name_of(a) {
        store(s, name, response, table);
    }
    Ok(Value::from(""))
}

/// `{% call noop_statement(name, message, code, rows_affected, res) %}`:
/// records a result without running anything.
pub fn noop_statement(s: &Shared, state: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    caller_output(state, a)?;
    if let Some(name) = name_of(a) {
        let (response, table) = raw_result(a, "res");
        store(s, name, response, table);
    }
    Ok(Value::from(""))
}

/// What the `{% call %}` block renders.
fn caller_output(state: &State<'_, '_>, a: &Args<'_>) -> Result<String, Error> {
    let caller = a
        .get(usize::MAX, "caller")
        .ok_or_else(|| jinja_error(format!("{}() is used as {{% call {}() %}}", a.name, a.name)))?;
    Ok(caller.call(state, &[], &[])?.to_string())
}

fn name_of(a: &Args<'_>) -> Option<String> {
    a.get(0, "name")
        .filter(|v| !v.is_none() && !v.is_undefined())
        .map(|v| v.to_string())
}

/// Runs `sql`: dbt's adapter response, and the rows if `fetch`.
fn run(s: &Shared, sql: &str, fetch: bool) -> Result<(Value, Value), Error> {
    let Some(db) = s.db_when_executing() else {
        return Err(jinja_error(if s.current.lock().unwrap().execute {
            "there is no database connection to run SQL on"
        } else {
            "SQL only runs while executing, not while parsing; guard it with {% if execute %}"
        }));
    };
    let (table, rows) = if fetch {
        let table = db.query(sql).map_err(to_jinja)?;
        let n = table.rows.len() as i64;
        (table, n)
    } else {
        db.execute(sql, &[]).map_err(to_jinja)?;
        (Table::default(), -1)
    };
    Ok((response("OK", "OK", rows), TableObject::value(table)))
}

/// dbt's `AdapterResponse`.
fn response(message: &str, code: &str, rows_affected: i64) -> Value {
    Value::from_serialize(serde_json::json!({
        "_message": message,
        "code": code,
        "rows_affected": rows_affected,
    }))
}

/// `store_result(name, response, agate_table)`.
pub fn store_result(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let name = a.string(0, "name")?;
    let response = a.get(1, "response").unwrap_or_default();
    let table = a.get(2, "agate_table").unwrap_or_default();
    store(s, name, response, table);
    Ok(Value::from(""))
}

/// `store_raw_result(name, message, code, rows_affected, agate_table)`.
pub fn store_raw_result(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let name = a.string(0, "name")?;
    let (response, table) = raw_result(a, "agate_table");
    store(s, name, response, table);
    Ok(Value::from(""))
}

/// The response and table of `(name, message, code, rows_affected, table)`
/// arguments, with the table argument called `table_key`.
fn raw_result(a: &Args<'_>, table_key: &str) -> (Value, Value) {
    let text = |pos, key| {
        a.get(pos, key)
            .filter(|v| !v.is_none())
            .map(|v| v.to_string())
            .unwrap_or_default()
    };
    let rows = a
        .get(3, "rows_affected")
        .and_then(|v| i64::try_from(v).ok())
        .unwrap_or(-1);
    let response = response(&text(1, "message"), &text(2, "code"), rows);
    (response, a.get(4, table_key).unwrap_or_default())
}

fn store(s: &Shared, name: String, response: Value, table: Value) {
    // `data` is the rows, as dbt's list of tuples: rows index by position.
    let data = if table.is_none() || table.is_undefined() {
        Value::from(Vec::<Value>::new())
    } else {
        table.clone()
    };
    let result = Value::from_iter([("response", response), ("table", table), ("data", data)]);
    s.current.lock().unwrap().results.insert(name, result);
}

/// `load_result(name)`: what `store_result` kept, or none.
pub fn load_result(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let name = a.string(0, "name")?;
    Ok(s.current
        .lock()
        .unwrap()
        .results
        .get(&name)
        .cloned()
        .unwrap_or(Value::from(())))
}

/// `load_relation(relation)`: the relation if it exists, else none.
pub fn load_relation(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let rel = a
        .get(0, "relation")
        .ok_or_else(|| jinja_error("load_relation(): missing relation"))?;
    let Some(r) = super::objects::relation_of(&rel) else {
        return Err(jinja_error("load_relation() takes a relation"));
    };
    let Some(db) = s.db_when_executing() else {
        return Ok(Value::from(()));
    };
    Ok(
        match db
            .relation_kind(&r.schema, &r.identifier)
            .map_err(to_jinja)?
        {
            Some(_) => rel.clone(),
            None => Value::from(()),
        },
    )
}
