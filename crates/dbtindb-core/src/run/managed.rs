//! The bookkeeping table: which relations dbtindb built. Only these are
//! ever dropped or rebuilt.

use std::sync::Arc;

use crate::context::Relation;
use crate::{Database, Dialect, Error};

pub struct Managed {
    db: Arc<dyn Database>,
    table: String,
}

impl Managed {
    pub fn new(db: Arc<dyn Database>) -> Self {
        let table = db.managed_table();
        Managed { db, table }
    }

    fn key(&self, rel: &Relation) -> String {
        let d = self.db.dialect();
        // SQLite resolves identifiers case-insensitively; so does the
        // table's NOCASE collation.
        format!(
            "schema_name = {} AND relation_name = {}",
            d.literal(&rel.schema),
            d.literal(&rel.identifier)
        )
    }

    pub fn contains(&self, rel: &Relation) -> Result<bool, Error> {
        let t = self.db.query(&format!(
            "SELECT 1 FROM {} WHERE {}",
            self.table,
            self.key(rel)
        ))?;
        Ok(!t.rows.is_empty())
    }

    pub fn checksum(&self, rel: &Relation) -> Result<Option<String>, Error> {
        let t = self.db.query(&format!(
            "SELECT checksum FROM {} WHERE {}",
            self.table,
            self.key(rel)
        ))?;
        Ok(t.rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_str())
            .map(str::to_owned))
    }

    pub fn forget(&self, rel: &Relation) -> Result<(), Error> {
        self.db.execute(
            &format!("DELETE FROM {} WHERE {}", self.table, self.key(rel)),
            &[],
        )
    }

    /// Records (or re-records) a relation built for `unique_id`.
    pub fn record(
        &self,
        rel: &Relation,
        unique_id: &str,
        materialized: &str,
        checksum: Option<&str>,
    ) -> Result<(), Error> {
        self.forget(rel)?;
        let p = |n| self.db.param(n);
        let now = match self.db.dialect() {
            Dialect::Postgres => "now()",
            Dialect::Sqlite => "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
        };
        self.db.execute(
            &format!(
                "INSERT INTO {} (schema_name, relation_name, unique_id, materialized, checksum, built_at) \
                 VALUES ({}, {}, {}, {}, {}, {now})",
                self.table,
                p(1),
                p(2),
                p(3),
                p(4),
                p(5)
            ),
            &[Some(&rel.schema), Some(&rel.identifier), Some(unique_id), Some(materialized), checksum],
        )
    }
}
