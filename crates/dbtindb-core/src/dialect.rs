//! How each supported database spells relations, literals and DDL.

/// The SQL dialect of the database dbtindb runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    Sqlite,
    DuckDB,
}

impl Dialect {
    /// The value of `target.type`, as dbt adapters name themselves.
    pub fn name(self) -> &'static str {
        match self {
            Dialect::Postgres => "postgres",
            Dialect::Sqlite => "sqlite",
            Dialect::DuckDB => "duckdb",
        }
    }

    /// The database's name for people, in error messages.
    pub fn display_name(self) -> &'static str {
        match self {
            Dialect::Postgres => "PostgreSQL",
            Dialect::Sqlite => "SQLite",
            Dialect::DuckDB => "DuckDB",
        }
    }

    /// Quotes an identifier. All dialects use standard double quotes, with
    /// embedded quotes doubled.
    pub fn quote(self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    /// A string literal. All dialects follow the standard: only `'` is
    /// special (PostgreSQL has standard_conforming_strings on by default
    /// since 9.1, and dbtindb's own statements never rely on it being off).
    pub fn literal(self, s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }

    /// The reference to a relation as it appears in compiled SQL.
    pub fn relation(self, schema: &str, name: &str) -> String {
        match self {
            Dialect::Postgres | Dialect::DuckDB => {
                format!("{}.{}", self.quote(schema), self.quote(name))
            }
            // A SQLite view may only reference objects in its own schema, so
            // qualifying with a schema name would break views over models.
            Dialect::Sqlite => self.quote(name),
        }
    }

    /// Whether models can be built in more than one schema.
    pub fn has_schemas(self) -> bool {
        self != Dialect::Sqlite
    }

    /// The longest identifier the database keeps without truncating it.
    pub fn max_identifier_bytes(self) -> Option<usize> {
        match self {
            Dialect::Postgres => Some(63),
            Dialect::Sqlite | Dialect::DuckDB => None,
        }
    }

    /// Creates a temporary table, visible only to this session, from a query.
    pub fn create_temp_table_as(self, name: &str, sql: &str) -> String {
        format!("CREATE TEMPORARY TABLE {} AS\n{sql}", self.quote(name))
    }

    /// The schema name temporary tables are looked up in.
    pub fn temp_schema(self) -> &'static str {
        match self {
            Dialect::Postgres => "pg_temp",
            // DuckDB keeps temporary tables in the `temp` catalog; its
            // Database implementation maps this name to that catalog.
            Dialect::Sqlite | Dialect::DuckDB => "temp",
        }
    }

    /// An expression for the current timestamp, fixed for the statement.
    pub fn now(self) -> &'static str {
        match self {
            Dialect::Postgres => "now()",
            Dialect::Sqlite => "strftime('%Y-%m-%d %H:%M:%f', 'now')",
            // A plain TIMESTAMP, like dbt-duckdb's snapshot timestamps;
            // now() is a TIMESTAMP WITH TIME ZONE.
            Dialect::DuckDB => "now()::timestamp",
        }
    }

    /// An expression hashing `expr` (text) to a hex digest, used for
    /// snapshots' `dbt_scd_id`. The SQLite extension registers
    /// `dbtindb_md5` because SQLite has no hash function of its own.
    pub fn md5(self, expr: &str) -> String {
        match self {
            Dialect::Postgres | Dialect::DuckDB => format!("md5({expr})"),
            Dialect::Sqlite => format!("dbtindb_md5({expr})"),
        }
    }

    /// `expr` as text, with NULL as the empty string, for hashing.
    pub fn text_or_empty(self, expr: &str) -> String {
        match self {
            Dialect::Postgres | Dialect::Sqlite | Dialect::DuckDB => {
                format!("coalesce(cast({expr} as text), '')")
            }
        }
    }

    /// A comparison that is true when `a` and `b` differ, treating two
    /// NULLs as equal.
    pub fn is_distinct(self, a: &str, b: &str) -> String {
        match self {
            Dialect::Postgres | Dialect::DuckDB => format!("{a} IS DISTINCT FROM {b}"),
            Dialect::Sqlite => format!("{a} IS NOT {b}"),
        }
    }

    /// Column types for seeds, by inferred kind.
    pub fn seed_type(self, kind: SeedType) -> &'static str {
        match (self, kind) {
            (Dialect::Postgres, SeedType::Integer) => "bigint",
            (Dialect::Postgres, SeedType::Number) => "numeric",
            (Dialect::Postgres, SeedType::Boolean) => "boolean",
            (Dialect::Postgres, SeedType::Date) => "date",
            (Dialect::Postgres, SeedType::Timestamp) => "timestamp",
            (Dialect::Postgres, SeedType::Text) => "text",
            (Dialect::Sqlite, SeedType::Integer | SeedType::Boolean) => "INTEGER",
            (Dialect::Sqlite, SeedType::Number) => "REAL",
            (Dialect::Sqlite, SeedType::Date | SeedType::Timestamp | SeedType::Text) => "TEXT",
            (Dialect::DuckDB, SeedType::Integer) => "BIGINT",
            // DuckDB's NUMERIC without precision is DECIMAL(18,3), which
            // would round seed values; DOUBLE is what dbt-duckdb infers.
            (Dialect::DuckDB, SeedType::Number) => "DOUBLE",
            (Dialect::DuckDB, SeedType::Boolean) => "BOOLEAN",
            (Dialect::DuckDB, SeedType::Date) => "DATE",
            (Dialect::DuckDB, SeedType::Timestamp) => "TIMESTAMP",
            (Dialect::DuckDB, SeedType::Text) => "VARCHAR",
        }
    }
}

/// The kinds of column a seed's values can be inferred as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedType {
    Integer,
    Number,
    Boolean,
    Date,
    Timestamp,
    Text,
}
