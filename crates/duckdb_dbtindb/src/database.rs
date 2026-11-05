//! dbtindb's view of the database, through the extension's own connection.

use std::sync::{Arc, Mutex};

use dbtindb_core::{Column, Database, Dialect, Error, RelationKind, Table};

use crate::duckdb::Connection;

/// The bookkeeping table; created by the first build. DuckDB resolves
/// identifiers case-insensitively, so the names compare that way too.
pub const MANAGED_DDL: &str = "CREATE TABLE IF NOT EXISTS main.dbtindb_managed (
    schema_name   VARCHAR COLLATE NOCASE NOT NULL,
    relation_name VARCHAR COLLATE NOCASE NOT NULL,
    unique_id     VARCHAR NOT NULL,
    materialized  VARCHAR NOT NULL,
    checksum      VARCHAR,
    built_at      TIMESTAMP WITH TIME ZONE NOT NULL,
    PRIMARY KEY (schema_name, relation_name)
)";

pub struct DuckDb {
    conn: Arc<Connection>,
    /// The catalog models are built in: the database's default one.
    catalog: String,
    /// Messages from `log()` and `exceptions.warn()`, returned with the
    /// results: a scalar function has no channel for notices.
    pub logs: Mutex<Vec<String>>,
}

impl DuckDb {
    pub fn new(conn: Arc<Connection>) -> Result<Self, Error> {
        let (_, rows) = conn
            .query("SELECT current_database()", &[])
            .map_err(Error::new)?;
        let catalog = rows
            .first()
            .and_then(|r| r[0].as_str())
            .unwrap_or("memory")
            .to_owned();
        Ok(DuckDb {
            conn,
            catalog,
            logs: Mutex::new(Vec::new()),
        })
    }

    pub fn query(&self, sql: &str, params: &[Option<&str>]) -> Result<Table, Error> {
        let (columns, rows) = self.conn.query(sql, params).map_err(Error::new)?;
        Ok(Table { columns, rows })
    }

    /// Where `schema` lives: temporary tables are in DuckDB's `temp`
    /// catalog (its `main` schema), which dbtindb calls schema `temp`.
    fn locate<'a>(&'a self, schema: &'a str) -> (&'a str, &'a str) {
        if schema == Dialect::DuckDB.temp_schema() {
            ("temp", "main")
        } else {
            (&self.catalog, schema)
        }
    }
}

impl Database for DuckDb {
    fn dialect(&self) -> Dialect {
        Dialect::DuckDB
    }

    fn query(&self, sql: &str) -> Result<Table, Error> {
        DuckDb::query(self, sql, &[])
    }

    fn execute(&self, sql: &str, params: &[Option<&str>]) -> Result<(), Error> {
        DuckDb::query(self, sql, params).map(drop)
    }

    fn schema_exists(&self, schema: &str) -> Result<bool, Error> {
        let (catalog, schema) = self.locate(schema);
        let t = DuckDb::query(
            self,
            "SELECT 1 FROM information_schema.schemata \
             WHERE catalog_name = $1 AND lower(schema_name) = lower($2)",
            &[Some(catalog), Some(schema)],
        )?;
        Ok(!t.rows.is_empty())
    }

    fn relation_kind(&self, schema: &str, name: &str) -> Result<Option<RelationKind>, Error> {
        let (catalog, schema) = self.locate(schema);
        let t = DuckDb::query(
            self,
            "SELECT table_type FROM information_schema.tables \
             WHERE table_catalog = $1 AND lower(table_schema) = lower($2) \
               AND lower(table_name) = lower($3)",
            &[Some(catalog), Some(schema), Some(name)],
        )?;
        Ok(t.rows.first().and_then(|r| r[0].as_str()).map(|t| match t {
            "BASE TABLE" | "LOCAL TEMPORARY" => RelationKind::Table,
            "VIEW" => RelationKind::View,
            other => RelationKind::Other(other.to_owned()),
        }))
    }

    fn columns(&self, schema: &str, name: &str) -> Result<Vec<Column>, Error> {
        let (catalog, schema) = self.locate(schema);
        let t = DuckDb::query(
            self,
            "SELECT column_name, data_type FROM information_schema.columns \
             WHERE table_catalog = $1 AND lower(table_schema) = lower($2) \
               AND lower(table_name) = lower($3) \
             ORDER BY ordinal_position",
            &[Some(catalog), Some(schema), Some(name)],
        )?;
        Ok(t.rows
            .into_iter()
            .map(|r| Column {
                name: r[0].as_str().unwrap_or_default().to_owned(),
                data_type: r[1].as_str().unwrap_or_default().to_owned(),
            })
            .collect())
    }

    fn managed_table(&self) -> String {
        "main.dbtindb_managed".into()
    }

    fn database_name(&self) -> String {
        self.catalog.clone()
    }

    fn log(&self, message: &str) {
        self.logs.lock().unwrap().push(message.to_owned());
    }
}
