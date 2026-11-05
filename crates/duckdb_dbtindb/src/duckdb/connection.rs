//! The extension's own connection to the database that loaded it.

use std::ffi::{CStr, CString, c_char};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use libduckdb_sys as ffi;
use serde_json::Value;

use super::function::{Function, drop_overload, execute};

/// A query's column names and rows.
pub type Rows = (Vec<String>, Vec<Vec<Value>>);

/// A connection the extension opened itself. DuckDB connections are not
/// safe for concurrent use, so every statement goes through the mutex.
pub struct Connection {
    conn: Mutex<ffi::duckdb_connection>,
    /// Held by the dbtindb call using the connection; see [`Connection::claim`].
    busy: AtomicBool,
}

// SAFETY: the handle is only used under the mutex.
unsafe impl Send for Connection {}
unsafe impl Sync for Connection {}

/// The connection, claimed by one dbtindb call until dropped.
pub struct Claim<'a>(&'a Connection);

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}

impl Connection {
    /// Connects to `db`.
    ///
    /// # Safety
    /// `db` must be the database DuckDB handed to the extension's entry
    /// point, during that call.
    pub unsafe fn connect(db: ffi::duckdb_database) -> Result<Self, String> {
        let mut conn: ffi::duckdb_connection = ptr::null_mut();
        // SAFETY: per this function's contract.
        if unsafe { ffi::duckdb_connect(db, &mut conn) } != ffi::duckdb_state_DuckDBSuccess {
            return Err("could not open a connection to the database".into());
        }
        Ok(Connection {
            conn: Mutex::new(conn),
            busy: AtomicBool::new(false),
        })
    }

    /// Claims the connection for one dbtindb call, or fails at once when
    /// another call has it. Waiting instead could deadlock: model SQL that
    /// calls a dbtindb function runs on this connection, from inside the
    /// call that holds it, possibly on another of DuckDB's threads.
    pub fn claim(&self) -> Result<Claim<'_>, String> {
        self.busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| Claim(self))
            .map_err(|_| "another dbtindb call is running on this database".to_owned())
    }

    /// Registers `function` with all its overloads, as volatile: they read
    /// tables and change the database.
    pub fn register(&self, function: Function) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: every handle below is created here and handed to DuckDB,
        // which copies what it keeps; the extra info boxes are owned by
        // DuckDB from then on and freed through `drop_overload`.
        unsafe {
            let mut set = ffi::duckdb_create_scalar_function_set(function.name.as_ptr());
            for overload in function.overloads {
                let mut f = ffi::duckdb_create_scalar_function();
                ffi::duckdb_scalar_function_set_name(f, function.name.as_ptr());
                for p in &overload.params {
                    ffi::duckdb_scalar_function_add_parameter(f, p.logical().0);
                }
                ffi::duckdb_scalar_function_set_return_type(f, overload.ret.logical().0);
                // NULL arguments reach the function: NULL options mean none.
                ffi::duckdb_scalar_function_set_special_handling(f);
                ffi::duckdb_scalar_function_set_volatile(f);
                ffi::duckdb_scalar_function_set_function(f, Some(execute));
                let info = Box::into_raw(Box::new(overload));
                ffi::duckdb_scalar_function_set_extra_info(f, info.cast(), Some(drop_overload));
                let added = ffi::duckdb_add_scalar_function_to_set(set, f);
                ffi::duckdb_destroy_scalar_function(&mut f);
                if added != ffi::duckdb_state_DuckDBSuccess {
                    ffi::duckdb_destroy_scalar_function_set(&mut set);
                    return Err(format!("could not add an overload of {:?}", function.name));
                }
            }
            let state = ffi::duckdb_register_scalar_function_set(*conn, set);
            ffi::duckdb_destroy_scalar_function_set(&mut set);
            if state != ffi::duckdb_state_DuckDBSuccess {
                return Err(format!("could not register {:?}", function.name));
            }
        }
        Ok(())
    }

    /// Runs `sql`, which may hold several statements when there are no
    /// parameters, and returns the last statement's rows. Parameters are
    /// bound as VARCHAR (`None` as NULL); DuckDB casts them to the type
    /// their place in the statement needs.
    pub fn query(&self, sql: &str, params: &[Option<&str>]) -> Result<Rows, String> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let sql = CString::new(sql).map_err(|_| "SQL contains a NUL byte".to_owned())?;
        // SAFETY: the connection is open and locked; every handle is
        // destroyed before returning.
        unsafe {
            let mut result: ffi::duckdb_result = std::mem::zeroed();
            let state = if params.is_empty() {
                ffi::duckdb_query(*conn, sql.as_ptr(), &mut result)
            } else {
                let mut stmt: ffi::duckdb_prepared_statement = ptr::null_mut();
                if ffi::duckdb_prepare(*conn, sql.as_ptr(), &mut stmt)
                    != ffi::duckdb_state_DuckDBSuccess
                {
                    let err = cstr(ffi::duckdb_prepare_error(stmt));
                    ffi::duckdb_destroy_prepare(&mut stmt);
                    return Err(err);
                }
                let bound = bind(stmt, params);
                let state = match bound {
                    Ok(()) => ffi::duckdb_execute_prepared(stmt, &mut result),
                    Err(_) => ffi::duckdb_state_DuckDBError,
                };
                ffi::duckdb_destroy_prepare(&mut stmt);
                bound?;
                state
            };
            if state != ffi::duckdb_state_DuckDBSuccess {
                let err = cstr(ffi::duckdb_result_error(&mut result));
                ffi::duckdb_destroy_result(&mut result);
                return Err(err);
            }
            let rows = read(&mut result);
            ffi::duckdb_destroy_result(&mut result);
            Ok(rows)
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let conn = self.conn.get_mut().unwrap_or_else(|p| p.into_inner());
        // SAFETY: opened by duckdb_connect and not used after this.
        unsafe { ffi::duckdb_disconnect(conn) };
    }
}

/// # Safety
/// `stmt` must be a successfully prepared statement.
unsafe fn bind(
    stmt: ffi::duckdb_prepared_statement,
    params: &[Option<&str>],
) -> Result<(), String> {
    for (i, p) in params.iter().enumerate() {
        let idx = i as u64 + 1;
        // SAFETY: per this function's contract; DuckDB copies the value.
        let state = unsafe {
            match p {
                None => ffi::duckdb_bind_null(stmt, idx),
                Some(text) => {
                    let text = CString::new(*text)
                        .map_err(|_| "a parameter contains a NUL byte".to_owned())?;
                    ffi::duckdb_bind_varchar(stmt, idx, text.as_ptr())
                }
            }
        };
        if state != ffi::duckdb_state_DuckDBSuccess {
            return Err(format!("could not bind parameter {idx}"));
        }
    }
    Ok(())
}

/// Reads a materialized result into JSON values: numbers and booleans as
/// themselves, every other type as DuckDB's text for it.
///
/// # Safety
/// `result` must be a successful, not yet destroyed result.
unsafe fn read(result: &mut ffi::duckdb_result) -> Rows {
    unsafe {
        let ncols = ffi::duckdb_column_count(result);
        let nrows = ffi::duckdb_row_count(result);
        let columns = (0..ncols)
            .map(|c| cstr(ffi::duckdb_column_name(result, c)))
            .collect();
        let types: Vec<_> = (0..ncols)
            .map(|c| ffi::duckdb_column_type(result, c))
            .collect();
        let rows = (0..nrows)
            .map(|r| {
                (0..ncols)
                    .map(|c| {
                        if ffi::duckdb_value_is_null(result, c, r) {
                            return Value::Null;
                        }
                        let p = ffi::duckdb_value_varchar(result, c, r);
                        let text = cstr(p);
                        ffi::duckdb_free(p.cast());
                        to_json(types[c as usize], text)
                    })
                    .collect()
            })
            .collect();
        (columns, rows)
    }
}

fn to_json(ty: ffi::duckdb_type, text: String) -> Value {
    use ffi::*;
    match ty {
        DUCKDB_TYPE_DUCKDB_TYPE_BOOLEAN => Value::Bool(text == "true"),
        DUCKDB_TYPE_DUCKDB_TYPE_TINYINT
        | DUCKDB_TYPE_DUCKDB_TYPE_SMALLINT
        | DUCKDB_TYPE_DUCKDB_TYPE_INTEGER
        | DUCKDB_TYPE_DUCKDB_TYPE_BIGINT
        | DUCKDB_TYPE_DUCKDB_TYPE_UTINYINT
        | DUCKDB_TYPE_DUCKDB_TYPE_USMALLINT
        | DUCKDB_TYPE_DUCKDB_TYPE_UINTEGER
        | DUCKDB_TYPE_DUCKDB_TYPE_UBIGINT
        | DUCKDB_TYPE_DUCKDB_TYPE_HUGEINT
        | DUCKDB_TYPE_DUCKDB_TYPE_UHUGEINT
        | DUCKDB_TYPE_DUCKDB_TYPE_FLOAT
        | DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE
        | DUCKDB_TYPE_DUCKDB_TYPE_DECIMAL => {
            // Integers beyond 64 bits and non-finite floats have no exact
            // JSON number; they stay text rather than lose their value.
            serde_json::from_str::<serde_json::Number>(&text)
                .map(Value::Number)
                .unwrap_or(Value::String(text))
        }
        _ => Value::String(text),
    }
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return "unknown DuckDB error".into();
    }
    // SAFETY: DuckDB returns NUL-terminated strings.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}
