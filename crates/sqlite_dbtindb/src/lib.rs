//! dbtindb for SQLite: a run-time loadable extension that builds dbt-style
//! Jinja SQL models from the `dbtindb_files` table, inside the database.

mod database;
mod functions;
mod sqlite;

use std::os::raw::{c_char, c_int};

use libsqlite3_sys as ffi;

/// Entry point SQLite looks for when loading `libsqlite_dbtindb` without
/// naming one: `sqlite3_` + the file name's letters, minus a `lib` prefix,
/// + `_init`.
///
/// # Safety
/// Called by SQLite with valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_sqlitedbtindb_init(
    db: *mut ffi::sqlite3,
    pz_err_msg: *mut *mut c_char,
    p_api: *mut ffi::sqlite3_api_routines,
) -> c_int {
    // SAFETY: SQLite's own pointers, passed straight through.
    unsafe {
        if !sqlite::init_api(p_api, pz_err_msg) {
            return ffi::SQLITE_ERROR;
        }
        match functions::register(db) {
            // The functions belong to this connection only; every new
            // connection loads the extension again.
            Ok(()) => ffi::SQLITE_OK,
            Err(msg) => {
                sqlite::set_error_message(pz_err_msg, &msg);
                ffi::SQLITE_ERROR
            }
        }
    }
}
