//! How each supported database spells relations and DDL.

/// The SQL dialect of the database dbtindb runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    Sqlite,
}

impl Dialect {
    /// The value of `target.type`, as dbt adapters name themselves.
    pub fn name(self) -> &'static str {
        match self {
            Dialect::Postgres => "postgres",
            Dialect::Sqlite => "sqlite",
        }
    }

    /// Quotes an identifier. Both dialects use standard double quotes, with
    /// embedded quotes doubled.
    pub fn quote(self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    /// The reference to a relation as it appears in compiled SQL.
    pub fn relation(self, schema: &str, name: &str) -> String {
        match self {
            Dialect::Postgres => format!("{}.{}", self.quote(schema), self.quote(name)),
            // A SQLite view may only reference objects in its own schema, so
            // qualifying with a schema name would break views over models.
            Dialect::Sqlite => self.quote(name),
        }
    }
}
