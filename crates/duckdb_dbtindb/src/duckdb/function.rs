//! Scalar functions: their declaration, and the callback DuckDB runs for
//! each chunk of rows.

use std::ffi::{CString, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use libduckdb_sys as ffi;

/// The SQL types the functions take and return.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Type {
    Varchar,
    /// VARCHAR with DuckDB's `JSON` alias. As a parameter, STRUCT and MAP
    /// arguments are cast to JSON text by DuckDB's json extension, so
    /// options can be written `{'select': 'orders+'}`; as a result, the
    /// value works with `->`, `->>` and `json_each`.
    Json,
}

impl Type {
    pub(super) fn logical(self) -> LogicalType {
        // SAFETY: creating and aliasing a fresh logical type.
        unsafe {
            let handle = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR);
            if self == Type::Json {
                ffi::duckdb_logical_type_set_alias(handle, c"JSON".as_ptr());
            }
            LogicalType(handle)
        }
    }
}

/// An owned logical type, destroyed on drop.
pub(super) struct LogicalType(pub(super) ffi::duckdb_logical_type);

impl Drop for LogicalType {
    fn drop(&mut self) {
        // SAFETY: created by duckdb_create_logical_type and not yet destroyed.
        unsafe { ffi::duckdb_destroy_logical_type(&mut self.0) };
    }
}

/// The text arguments of one row: `None` for NULL, and for arguments the
/// overload does not have.
pub struct Args<'a>(Vec<Option<&'a [u8]>>);

impl<'a> Args<'a> {
    pub fn get(&self, i: usize) -> Option<&'a [u8]> {
        self.0.get(i).copied().flatten()
    }
}

/// Computes one row's result; `None` is NULL.
pub type Callback = dyn Fn(&Args<'_>) -> Result<Option<String>, String> + Send + Sync;

/// What DuckDB keeps for one overload.
pub(super) struct Overload {
    pub(super) params: Vec<Type>,
    pub(super) ret: Type,
    pub(super) f: Arc<Callback>,
}

/// A volatile function and its overloads, registered with
/// [`super::Connection::register`].
pub struct Function {
    pub(super) name: CString,
    pub(super) overloads: Vec<Overload>,
}

impl Function {
    pub fn new(name: &str) -> Self {
        Function {
            name: CString::new(name).expect("function names have no NUL"),
            overloads: Vec::new(),
        }
    }

    /// Adds an overload taking `params` and returning `ret`.
    pub fn overload(mut self, params: &[Type], ret: Type, f: Arc<Callback>) -> Self {
        self.overloads.push(Overload {
            params: params.to_vec(),
            ret,
            f,
        });
        self
    }
}

pub(super) unsafe extern "C" fn drop_overload(p: *mut c_void) {
    // SAFETY: `p` is the box `register` leaked for this overload.
    drop(unsafe { Box::from_raw(p.cast::<Overload>()) });
}

/// Runs an overload over one input chunk.
pub(super) unsafe extern "C" fn execute(
    info: ffi::duckdb_function_info,
    input: ffi::duckdb_data_chunk,
    output: ffi::duckdb_vector,
) {
    // A panic must not unwind into DuckDB: it becomes the query's error.
    let result = catch_unwind(AssertUnwindSafe(|| unsafe {
        execute_chunk(info, input, output)
    }))
    .unwrap_or_else(|_| Err("internal error: the dbtindb extension panicked".into()));
    if let Err(msg) = result {
        let msg = CString::new(msg.replace('\0', " ")).expect("NULs were replaced");
        // SAFETY: `info` is this call's function info.
        unsafe { ffi::duckdb_scalar_function_set_error(info, msg.as_ptr()) };
    }
}

/// # Safety
/// Called by DuckDB with the function info, a flattened input chunk and the
/// output vector of one call.
unsafe fn execute_chunk(
    info: ffi::duckdb_function_info,
    input: ffi::duckdb_data_chunk,
    output: ffi::duckdb_vector,
) -> Result<(), String> {
    unsafe {
        let overload = &*ffi::duckdb_scalar_function_get_extra_info(info).cast::<Overload>();
        let rows = ffi::duckdb_data_chunk_get_size(input) as usize;
        let columns: Vec<_> = (0..overload.params.len())
            .map(|i| {
                let v = ffi::duckdb_data_chunk_get_vector(input, i as u64);
                (
                    ffi::duckdb_vector_get_data(v).cast::<ffi::duckdb_string_t>(),
                    ffi::duckdb_vector_get_validity(v),
                )
            })
            .collect();
        for row in 0..rows {
            // Every parameter is VARCHAR and the input is flat, so each data
            // array holds a string at `row` unless the validity mask says NULL.
            let args = Args(
                columns
                    .iter()
                    .map(|&(data, validity)| {
                        if !validity.is_null()
                            && !ffi::duckdb_validity_row_is_valid(validity, row as u64)
                        {
                            return None;
                        }
                        let s = data.add(row);
                        let len = ffi::duckdb_string_t_length(*s) as usize;
                        Some(std::slice::from_raw_parts(
                            ffi::duckdb_string_t_data(s).cast::<u8>(),
                            len,
                        ))
                    })
                    .collect(),
            );
            match (overload.f)(&args)? {
                None => {
                    ffi::duckdb_vector_ensure_validity_writable(output);
                    ffi::duckdb_validity_set_row_invalid(
                        ffi::duckdb_vector_get_validity(output),
                        row as u64,
                    );
                }
                Some(s) => ffi::duckdb_vector_assign_string_element_len(
                    output,
                    row as u64,
                    s.as_ptr().cast(),
                    s.len() as u64,
                ),
            }
        }
        Ok(())
    }
}
