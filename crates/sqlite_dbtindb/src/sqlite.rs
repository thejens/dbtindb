//! A minimal, safe wrapper over the parts of the SQLite C API this extension
//! uses: registering scalar functions, reading their arguments, returning
//! text or NULL, and running statements on the calling connection.
//!
//! Everything goes through the routine table SQLite hands a loadable
//! extension, via `libsqlite3-sys`. Only APIs from SQLite 3.30 or earlier
//! are used, so the minimum version is set by the headers the bindings are
//! generated from (see sqlite-headers/).

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use libsqlite3_sys as ffi;
use serde_json::Value;

/// One SQL argument value. Borrowed text is valid for the duration of the
/// function call it was read in; other values are only told apart by type.
#[derive(Clone, Copy)]
pub enum Arg<'a> {
    Null,
    Text(&'a [u8]),
    Other(&'static str),
}

/// Argument access for one call of a scalar function.
pub struct Call<'a> {
    db: *mut ffi::sqlite3,
    args: &'a [*mut ffi::sqlite3_value],
}

impl<'a> Call<'a> {
    /// Argument `i`, or `Arg::Null` when the call has fewer arguments.
    pub fn arg(&self, i: usize) -> Arg<'a> {
        let Some(&v) = self.args.get(i) else {
            return Arg::Null;
        };
        // SAFETY: `v` is a live argument of the current call. The text
        // pointer must be read *before* the length: sqlite3_value_bytes may
        // convert the value, invalidating an earlier pointer, but not one
        // fetched first.
        unsafe {
            match ffi::sqlite3_value_type(v) {
                ffi::SQLITE_TEXT => {
                    let p = ffi::sqlite3_value_text(v);
                    let n = ffi::sqlite3_value_bytes(v);
                    Arg::Text(slice(p.cast(), n))
                }
                ffi::SQLITE_NULL => Arg::Null,
                ffi::SQLITE_INTEGER => Arg::Other("integer"),
                ffi::SQLITE_FLOAT => Arg::Other("real"),
                _ => Arg::Other("blob"),
            }
        }
    }

    /// Argument `i` as SQL would cast it to text, or `None` for NULL.
    pub fn text(&self, i: usize) -> Option<&'a [u8]> {
        let &v = self.args.get(i)?;
        // SAFETY: as in `arg`; sqlite3_value_text converts numbers to text
        // in place, and the pointer is read before the length.
        unsafe {
            if ffi::sqlite3_value_type(v) == ffi::SQLITE_NULL {
                return None;
            }
            let p = ffi::sqlite3_value_text(v);
            let n = ffi::sqlite3_value_bytes(v);
            Some(slice(p.cast(), n))
        }
    }

    /// Argument `i` as SQL would cast it to an integer, or `None` for NULL.
    pub fn int(&self, i: usize) -> Option<i64> {
        let &v = self.args.get(i)?;
        // SAFETY: `v` is a live argument of the current call.
        unsafe {
            if ffi::sqlite3_value_type(v) == ffi::SQLITE_NULL {
                return None;
            }
            Some(ffi::sqlite3_value_int64(v))
        }
    }

    /// The connection the function was called on.
    pub fn db(&self) -> Db {
        Db(self.db)
    }
}

/// The connection a function call came from.
///
/// It can only be obtained from a [`Call`], so it is only used while SQLite
/// is inside one of this connection's own function calls: on the thread
/// that holds the connection, with the connection open.
#[derive(Clone, Copy)]
pub struct Db(*mut ffi::sqlite3);

impl Db {
    /// Runs one statement with text parameters (`None` binds NULL) and
    /// returns its column names and every row, values typed as JSON.
    pub fn query(
        &self,
        sql: &str,
        params: &[Option<&str>],
    ) -> Result<(Vec<String>, Vec<Vec<Value>>), String> {
        let stmt = self.prepare(sql)?;
        for (i, p) in params.iter().enumerate() {
            let n = c_int::try_from(i + 1).expect("few parameters");
            // SAFETY: valid statement; SQLite copies the text (TRANSIENT).
            let rc = unsafe {
                match p {
                    Some(p) => ffi::sqlite3_bind_text(
                        stmt.0,
                        n,
                        p.as_ptr().cast(),
                        len_c_int(p.len()),
                        ffi::SQLITE_TRANSIENT(),
                    ),
                    None => ffi::sqlite3_bind_null(stmt.0, n),
                }
            };
            if rc != ffi::SQLITE_OK {
                return Err(self.errmsg());
            }
        }
        // SAFETY: valid statement.
        let width = unsafe { ffi::sqlite3_column_count(stmt.0) };
        let columns = (0..width)
            .map(|c| {
                // SAFETY: `c` is a valid column index of a prepared statement.
                let p = unsafe { ffi::sqlite3_column_name(stmt.0, c) };
                if p.is_null() {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
                }
            })
            .collect();
        let mut rows = Vec::new();
        loop {
            // SAFETY: valid statement, stepped on its own connection.
            match unsafe { ffi::sqlite3_step(stmt.0) } {
                ffi::SQLITE_ROW => rows.push(
                    (0..width)
                        .map(|c| unsafe { column(stmt.0, c) })
                        .collect::<Result<_, _>>()?,
                ),
                ffi::SQLITE_DONE => return Ok((columns, rows)),
                _ => return Err(self.errmsg()),
            }
        }
    }

    /// Runs SQL without parameters or results; may hold several statements.
    pub fn exec(&self, sql: &str) -> Result<(), String> {
        let sql = CString::new(sql).map_err(|_| "SQL contains a NUL byte".to_owned())?;
        // SAFETY: valid connection; no callback, so no error string is
        // allocated and the message comes from sqlite3_errmsg instead.
        let rc = unsafe {
            ffi::sqlite3_exec(self.0, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
        };
        if rc == ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(self.errmsg())
        }
    }

    fn prepare(&self, sql: &str) -> Result<Stmt, String> {
        let mut stmt = ptr::null_mut();
        // SAFETY: valid connection; the length bounds the SQL text, so it
        // needs no NUL terminator.
        let rc = unsafe {
            ffi::sqlite3_prepare_v2(
                self.0,
                sql.as_ptr().cast(),
                len_c_int(sql.len()),
                &mut stmt,
                ptr::null_mut(),
            )
        };
        if rc != ffi::SQLITE_OK {
            // A failed prepare may still hand back a statement to finalize.
            drop(Stmt(stmt));
            return Err(self.errmsg());
        }
        Ok(Stmt(stmt))
    }

    fn errmsg(&self) -> String {
        // SAFETY: sqlite3_errmsg always returns a valid string for an open db.
        unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(self.0)) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Column `c` of the current row, as JSON.
///
/// # Safety
/// `stmt` must have a current row.
unsafe fn column(stmt: *mut ffi::sqlite3_stmt, c: c_int) -> Result<Value, String> {
    // SAFETY: forwarded; text is read pointer first, then length.
    unsafe {
        Ok(match ffi::sqlite3_column_type(stmt, c) {
            ffi::SQLITE_INTEGER => Value::from(ffi::sqlite3_column_int64(stmt, c)),
            ffi::SQLITE_FLOAT => serde_json::Number::from_f64(ffi::sqlite3_column_double(stmt, c))
                .map_or(Value::Null, Value::Number),
            ffi::SQLITE_NULL => Value::Null,
            // Text, and blobs shown as their text.
            _ => {
                let p = ffi::sqlite3_column_text(stmt, c);
                let n = ffi::sqlite3_column_bytes(stmt, c);
                Value::from(String::from_utf8_lossy(slice(p.cast(), n)).into_owned())
            }
        })
    }
}

/// A prepared statement, finalized when dropped so that nothing prepared
/// by a function call outlives it.
struct Stmt(*mut ffi::sqlite3_stmt);

impl Drop for Stmt {
    fn drop(&mut self) {
        // SAFETY: finalizing null is a no-op; otherwise the statement is
        // finalized exactly once.
        unsafe { ffi::sqlite3_finalize(self.0) };
    }
}

/// A scalar function's implementation. `Ok(Some)` becomes a text result,
/// `Ok(None)` NULL, and `Err` a SQL error with that message.
pub type ScalarFn = Box<dyn Fn(&Call<'_>) -> Result<Option<String>, String>>;

/// Registers a scalar function on `db`. The closure is freed by SQLite when
/// the function is replaced or the connection closes.
///
/// # Safety
/// `db` must be an open connection.
pub unsafe fn create_function(
    db: *mut ffi::sqlite3,
    name: &CStr,
    n_arg: c_int,
    flags: c_int,
    f: ScalarFn,
) -> Result<(), String> {
    let app = Box::into_raw(Box::new(f));
    // SAFETY: `app` is a valid boxed closure whose ownership passes to
    // SQLite, which frees it with `destroy`, including when registration
    // fails.
    let rc = unsafe {
        ffi::sqlite3_create_function_v2(
            db,
            name.as_ptr(),
            n_arg,
            flags,
            app.cast(),
            Some(trampoline),
            None,
            None,
            Some(destroy),
        )
    };
    if rc == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(Db(db).errmsg())
    }
}

unsafe extern "C" fn trampoline(
    ctx: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    // SAFETY: SQLite passes the pointer registered in `create_function` and
    // an argument array of `argc` live values.
    let (f, call) = unsafe {
        let f = &*ffi::sqlite3_user_data(ctx).cast::<ScalarFn>();
        let args: &[*mut ffi::sqlite3_value] = if argc == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(argv, argc as usize)
        };
        let db = ffi::sqlite3_context_db_handle(ctx);
        (f, Call { db, args })
    };
    // A panic must not unwind into SQLite's C frames.
    let result = catch_unwind(AssertUnwindSafe(|| f(&call)))
        .unwrap_or_else(|_| Err("internal error in dbtindb (panic)".to_owned()));
    // SAFETY: valid context; results are copied by SQLite (TRANSIENT).
    unsafe {
        match result {
            Ok(None) => ffi::sqlite3_result_null(ctx),
            Ok(Some(s)) => ffi::sqlite3_result_text64(
                ctx,
                s.as_ptr().cast(),
                s.len() as ffi::sqlite3_uint64,
                ffi::SQLITE_TRANSIENT(),
                ffi::SQLITE_UTF8 as u8,
            ),
            Err(msg) => ffi::sqlite3_result_error(ctx, msg.as_ptr().cast(), len_c_int(msg.len())),
        }
    }
}

unsafe extern "C" fn destroy(app: *mut c_void) {
    // SAFETY: `app` came from `Box::into_raw` in `create_function`.
    drop(unsafe { Box::from_raw(app.cast::<ScalarFn>()) });
}

/// Initialises the API routine table handed to a loadable extension. Must
/// run before any other SQLite call.
///
/// On failure the message is already stored in `pz_err_msg`. It is written
/// through the routine table's own `mprintf`, because the wrapper functions
/// are not usable until initialisation succeeds.
///
/// # Safety
/// `p_api` and `pz_err_msg` must be the pointers SQLite passed to the entry
/// point.
pub unsafe fn init_api(
    p_api: *mut ffi::sqlite3_api_routines,
    pz_err_msg: *mut *mut c_char,
) -> bool {
    if p_api.is_null() {
        return false;
    }
    // SAFETY: `p_api` is SQLite's routine table.
    let Err(err) = (unsafe { ffi::rusqlite_extension_init2(p_api) }) else {
        return true;
    };
    let msg = match err {
        ffi::InitError::VersionMismatch {
            compile_time,
            runtime,
        } => format!(
            "dbtindb requires SQLite {} or newer, but this is SQLite {}",
            version_string(compile_time),
            version_string(runtime)
        ),
        other => format!("dbtindb could not initialise the SQLite API: {other:?}"),
    };
    // SAFETY: `mprintf` comes straight from the routine table; "%s" with a
    // valid NUL-terminated string.
    unsafe {
        if let (false, Some(mprintf)) = (pz_err_msg.is_null(), (*p_api).mprintf) {
            let msg = CString::new(msg).unwrap_or_default();
            *pz_err_msg = mprintf(c"%s".as_ptr(), msg.as_ptr());
        }
    }
    false
}

fn version_string(n: c_int) -> String {
    format!("{}.{}.{}", n / 1_000_000, n / 1000 % 1000, n % 1000)
}

/// Copies `msg` into SQLite-allocated memory for an entry point's
/// `pzErrMsg`, which SQLite frees.
///
/// # Safety
/// Only after the API table is initialised; `pz_err_msg` may be null.
pub unsafe fn set_error_message(pz_err_msg: *mut *mut c_char, msg: &str) {
    if pz_err_msg.is_null() {
        return;
    }
    // sqlite3_mprintf is variadic and has no generated wrapper, so copy the
    // bytes into sqlite3_malloc64 memory, which is what SQLite frees.
    let bytes = msg.replace('\0', " ");
    // SAFETY: allocating len + 1 bytes and writing exactly that many.
    unsafe {
        let buf = ffi::sqlite3_malloc64(bytes.len() as ffi::sqlite3_uint64 + 1).cast::<u8>();
        if buf.is_null() {
            return;
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
        *buf.add(bytes.len()) = 0;
        *pz_err_msg = buf.cast();
    }
}

/// Builds a slice from a pointer and a length SQLite returned; a null
/// pointer (an empty value) becomes an empty slice.
unsafe fn slice<'a>(p: *const u8, n: c_int) -> &'a [u8] {
    if p.is_null() || n <= 0 {
        &[]
    } else {
        // SAFETY: SQLite guarantees `n` readable bytes at `p`.
        unsafe { std::slice::from_raw_parts(p, n as usize) }
    }
}

/// Lengths passed to SQLite are C ints; anything larger is truncated, which
/// for error messages is harmless.
fn len_c_int(n: usize) -> c_int {
    c_int::try_from(n).unwrap_or(c_int::MAX)
}
