//! A minimal, safe wrapper over the parts of DuckDB's C extension API this
//! extension uses: registering scalar functions over text, and running
//! statements on the extension's own connection.

mod connection;
mod function;

pub use connection::Connection;
pub use function::{Args, Function, Type};
