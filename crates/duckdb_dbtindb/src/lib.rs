//! dbtindb for DuckDB: a loadable extension that builds dbt-style Jinja SQL
//! models from the `dbtindb_files` table, inside the database.
//!
//! It is built on DuckDB's stable C extension API. DuckDB finds it by the
//! file name `dbtindb.duckdb_extension` and calls [`dbtindb_init_c_api`].

mod database;
mod duckdb;
mod functions;

use std::ffi::CString;
use std::sync::Arc;

use libduckdb_sys as ffi;

/// The C API version the extension is built against. DuckDB refuses to load
/// it into a release that does not provide this version.
const C_API_VERSION: &str = "v1.5.6";

/// The entry point DuckDB calls when loading the extension: its name plus
/// `_init_c_api`.
///
/// # Safety
/// Called by DuckDB with valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dbtindb_init_c_api(
    info: ffi::duckdb_extension_info,
    access: *const ffi::duckdb_extension_access,
) -> bool {
    // SAFETY: DuckDB's own pointers, passed straight through.
    let result = std::panic::catch_unwind(|| unsafe { init(info, access) }).unwrap_or_else(|_| {
        Err("internal error: the dbtindb extension panicked while loading".into())
    });
    match result {
        Ok(loaded) => loaded,
        Err(msg) => {
            // SAFETY: as above; `set_error` copies the message.
            unsafe {
                if let Some(set_error) = (*access).set_error {
                    let msg = CString::new(msg.replace('\0', " ")).expect("NULs were replaced");
                    set_error(info, msg.as_ptr());
                }
            }
            false
        }
    }
}

/// Loads the extension. `Ok(false)` means DuckDB refused the API version
/// and has recorded why, so no message of ours should replace its own.
///
/// # Safety
/// `info` and `access` must be what DuckDB passed to the entry point.
unsafe fn init(
    info: ffi::duckdb_extension_info,
    access: *const ffi::duckdb_extension_access,
) -> Result<bool, String> {
    unsafe {
        match ffi::duckdb_rs_extension_api_init(info, access, C_API_VERSION) {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(e) => return Err(e.to_owned()),
        }
        let get_database = (*access)
            .get_database
            .ok_or("DuckDB did not provide get_database")?;
        let db = get_database(info);
        if db.is_null() {
            return Err("DuckDB did not provide the database".into());
        }
        // The database handle is only valid during this call, so the one
        // connection everything runs on is opened now. It lives as long as
        // the functions, which DuckDB keeps until the database is closed.
        let conn = Arc::new(duckdb::Connection::connect(*db)?);
        functions::register(conn)?;
        Ok(true)
    }
}
