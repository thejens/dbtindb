//! What dbtindb needs from the database it runs in.

use serde_json::Value;

use crate::{Dialect, Error};

/// The result of a query: column names and rows of JSON values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// A column of an existing relation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
}

/// What a name in a schema refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelationKind {
    View,
    Table,
    MaterializedView,
    /// Anything else (a sequence, an index, a foreign table, ...), named.
    Other(String),
}

impl RelationKind {
    /// The keyword `DROP` takes for this kind, if dbtindb can drop it.
    pub fn drop_keyword(&self) -> Option<&'static str> {
        match self {
            RelationKind::View => Some("VIEW"),
            RelationKind::Table => Some("TABLE"),
            RelationKind::MaterializedView => Some("MATERIALIZED VIEW"),
            RelationKind::Other(_) => None,
        }
    }

    pub fn describe(&self) -> &str {
        match self {
            RelationKind::View => "view",
            RelationKind::Table => "table",
            RelationKind::MaterializedView => "materialized view",
            RelationKind::Other(what) => what,
        }
    }
}

/// The connection dbtindb runs on.
///
/// Implementations are handed to the Jinja environment, whose functions
/// must be `Send + Sync + 'static`. Database connections are neither, but
/// dbtindb only ever uses them from the thread that called it, for the
/// duration of that call; implementations assert that with `unsafe impl`.
pub trait Database: Send + Sync + 'static {
    fn dialect(&self) -> Dialect;

    /// Runs a query and returns all its rows.
    fn query(&self, sql: &str) -> Result<Table, Error>;

    /// Runs a statement with text parameters (`$1`, `$2`, ... on
    /// PostgreSQL, `?1`, `?2`, ... on SQLite; see [`Database::param`]).
    /// `None` binds NULL.
    fn execute(&self, sql: &str, params: &[Option<&str>]) -> Result<(), Error>;

    /// The placeholder for parameter `n` (1-based).
    fn param(&self, n: usize) -> String {
        match self.dialect() {
            Dialect::Postgres => format!("${n}"),
            Dialect::Sqlite => format!("?{n}"),
        }
    }

    /// Whether `schema` exists.
    fn schema_exists(&self, schema: &str) -> Result<bool, Error>;

    /// What exists at `schema.name`, if anything.
    fn relation_kind(&self, schema: &str, name: &str) -> Result<Option<RelationKind>, Error>;

    /// The columns of `schema.name`, in order.
    fn columns(&self, schema: &str, name: &str) -> Result<Vec<Column>, Error>;

    /// The quoted, qualified name of the bookkeeping table that records
    /// the relations dbtindb built.
    fn managed_table(&self) -> String;

    /// The current database's name, for `target.database`.
    fn database_name(&self) -> String;

    /// Reports a message from `log()` or `exceptions.warn()`.
    fn log(&self, message: &str);
}
