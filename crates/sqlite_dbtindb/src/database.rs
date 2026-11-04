//! dbtindb's view of the SQLite connection a function was called on.

use std::sync::Mutex;

use dbtindb_core::{Column, Database, Dialect, Error, RelationKind, Table};

use crate::sqlite::Db;

pub struct SqliteDb {
    db: Db,
    /// Messages from `log()` and `exceptions.warn()`, returned with the
    /// results: SQLite has no channel for notices.
    pub logs: Mutex<Vec<String>>,
}

impl SqliteDb {
    pub fn new(db: Db) -> Self {
        SqliteDb {
            db,
            logs: Mutex::new(Vec::new()),
        }
    }
}

// SAFETY: the connection is only used during the SQL function call that
// created this value, on the thread SQLite called it on; dbtindb never
// moves work to other threads. The bounds exist because Jinja functions
// must be Send + Sync.
unsafe impl Send for SqliteDb {}
unsafe impl Sync for SqliteDb {}

/// The bookkeeping table; created by the first build.
pub const MANAGED_DDL: &str = "CREATE TABLE IF NOT EXISTS main.dbtindb_managed (
    schema_name   TEXT NOT NULL,
    relation_name TEXT NOT NULL COLLATE NOCASE,
    unique_id     TEXT NOT NULL,
    materialized  TEXT NOT NULL,
    checksum      TEXT,
    built_at      TEXT NOT NULL,
    PRIMARY KEY (schema_name, relation_name)
)";

impl Database for SqliteDb {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    fn query(&self, sql: &str) -> Result<Table, Error> {
        let (columns, rows) = self.db.query(sql, &[]).map_err(Error::new)?;
        Ok(Table { columns, rows })
    }

    fn execute(&self, sql: &str, params: &[Option<&str>]) -> Result<(), Error> {
        if params.is_empty() {
            // Hooks may hold several statements.
            self.db.exec(sql).map_err(Error::new)
        } else {
            self.db.query(sql, params).map(drop).map_err(Error::new)
        }
    }

    fn schema_exists(&self, schema: &str) -> Result<bool, Error> {
        let (_, rows) = self
            .db
            .query(
                "SELECT 1 FROM pragma_database_list WHERE name = ?1",
                &[Some(schema)],
            )
            .map_err(Error::new)?;
        Ok(!rows.is_empty())
    }

    fn relation_kind(&self, schema: &str, name: &str) -> Result<Option<RelationKind>, Error> {
        let master = if schema == "temp" {
            "temp.sqlite_master"
        } else {
            "main.sqlite_master"
        };
        let (_, rows) = self
            .db
            .query(
                &format!("SELECT type FROM {master} WHERE name = ?1 COLLATE NOCASE"),
                &[Some(name)],
            )
            .map_err(Error::new)?;
        Ok(rows.first().and_then(|r| r[0].as_str()).map(|t| match t {
            "table" => RelationKind::Table,
            "view" => RelationKind::View,
            other => RelationKind::Other(other.to_owned()),
        }))
    }

    fn columns(&self, schema: &str, name: &str) -> Result<Vec<Column>, Error> {
        let (_, rows) = self
            .db
            .query(
                "SELECT name, type FROM pragma_table_info(?1, ?2) ORDER BY cid",
                &[Some(name), Some(schema)],
            )
            .map_err(Error::new)?;
        Ok(rows
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
        "main".into()
    }

    fn log(&self, message: &str) {
        self.logs.lock().unwrap().push(message.to_owned());
    }
}
