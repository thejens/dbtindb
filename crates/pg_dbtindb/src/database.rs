//! dbtindb's view of the PostgreSQL backend it runs in, through SPI.

use std::ffi::CStr;

use dbtindb_core::{Column, Database, Dialect, Error, RelationKind, Table};
use pgrx::datum::DatumWithOid;
use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::spi::{SpiClient, SpiError};
use serde_json::{Number, Value};

/// The backend's SPI. Every call opens its own SPI connection, so values
/// never outlive the memory context SPI allocated them in.
pub struct PgDb {
    managed_table: String,
    database_name: String,
}

// SAFETY: a `PgDb` lives for one SQL function call and is only used on the
// backend's single thread; dbtindb never moves work to other threads. The
// bounds exist because Jinja functions must be Send + Sync.
unsafe impl Send for PgDb {}
unsafe impl Sync for PgDb {}

impl PgDb {
    pub fn new() -> Result<Self, Error> {
        Spi::connect(|client| {
            let managed_table = extension_table(client, "pg_dbtindb", "dbtindb_managed")?;
            let database_name = first_text(client, "SELECT current_database()::text", &[])?
                .expect("there is always a current database");
            Ok(PgDb {
                managed_table,
                database_name,
            })
        })
    }
}

impl Database for PgDb {
    fn dialect(&self) -> Dialect {
        Dialect::Postgres
    }

    fn query(&self, sql: &str) -> Result<Table, Error> {
        Spi::connect_mut(|client| {
            // Writable: run_query() may run anything, and a read-only SPI
            // call could not see what earlier writes in this call did.
            let mut rows = client
                .update(sql, None, &[])
                .map_err(|e| spi_error(e, sql))?;
            // Utility statements (DDL) have no tuple descriptor.
            let Ok(width) = rows.columns() else {
                return Ok(Table::default());
            };
            let mut columns = Vec::with_capacity(width);
            let mut readers = Vec::with_capacity(width);
            for i in 1..=width {
                columns.push(rows.column_name(i).map_err(|e| spi_error(e, sql))?);
                readers.push(Reader::new(
                    rows.column_type_oid(i)
                        .map_err(|e| spi_error(e, sql))?
                        .value(),
                ));
            }
            let mut table = Table {
                columns,
                rows: Vec::with_capacity(rows.len()),
            };
            while rows.next().is_some() {
                let mut row = Vec::with_capacity(width);
                for (i, reader) in readers.iter().enumerate() {
                    let datum = rows
                        .get_datum_by_ordinal(i + 1)
                        .map_err(|e| spi_error(e, sql))?;
                    row.push(datum.map_or(Value::Null, |d| reader.read(d)));
                }
                table.rows.push(row);
            }
            Ok(table)
        })
    }

    fn execute(&self, sql: &str, params: &[Option<&str>]) -> Result<(), Error> {
        // Without parameters SPI accepts several `;`-separated statements,
        // which hooks may hold.
        let args: Vec<DatumWithOid> = params.iter().map(|&p| p.into()).collect();
        Spi::connect_mut(|client| {
            client
                .update(sql, None, &args)
                .map(drop)
                .map_err(|e| spi_error(e, sql))
        })
    }

    fn schema_exists(&self, schema: &str) -> Result<bool, Error> {
        Spi::connect(|client| {
            Ok(first_text(
                client,
                "SELECT nspname::text FROM pg_catalog.pg_namespace WHERE nspname = $1",
                &[schema.into()],
            )?
            .is_some())
        })
    }

    fn relation_kind(&self, schema: &str, name: &str) -> Result<Option<RelationKind>, Error> {
        let relkind = Spi::connect(|client| {
            first_text(
                client,
                &format!(
                    "SELECT c.relkind::text FROM pg_catalog.pg_class c WHERE {}",
                    relation_filter(schema)
                ),
                &[schema.into(), name.into()],
            )
        })?;
        Ok(relkind.map(|k| match k.as_str() {
            "v" => RelationKind::View,
            "r" => RelationKind::Table,
            "m" => RelationKind::MaterializedView,
            "p" => RelationKind::Other("partitioned table".into()),
            "f" => RelationKind::Other("foreign table".into()),
            "S" => RelationKind::Other("sequence".into()),
            "i" | "I" => RelationKind::Other("index".into()),
            "c" => RelationKind::Other("composite type".into()),
            other => RelationKind::Other(format!("relation of kind {other:?}")),
        }))
    }

    fn columns(&self, schema: &str, name: &str) -> Result<Vec<Column>, Error> {
        let sql = format!(
            "SELECT a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod)
               FROM pg_catalog.pg_class c
               JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid
              WHERE {} AND a.attnum > 0 AND NOT a.attisdropped
              ORDER BY a.attnum",
            relation_filter(schema)
        );
        Spi::connect(|client| {
            let rows = client
                .select(&sql, None, &[schema.into(), name.into()])
                .map_err(|e| spi_error(e, &sql))?;
            rows.map(|row| {
                Ok(Column {
                    name: row
                        .get::<String>(1)
                        .map_err(|e| spi_error(e, &sql))?
                        .unwrap_or_default(),
                    data_type: row
                        .get::<String>(2)
                        .map_err(|e| spi_error(e, &sql))?
                        .unwrap_or_default(),
                })
            })
            .collect()
        })
    }

    fn managed_table(&self) -> String {
        self.managed_table.clone()
    }

    fn database_name(&self) -> String {
        self.database_name.clone()
    }

    fn log(&self, message: &str) {
        notice!("{message}");
    }
}

/// Matches `pg_class c` by schema `$1` and name `$2`. `pg_temp` is an alias
/// for this session's temporary schema, whose real name is `pg_temp_N`.
fn relation_filter(schema: &str) -> &'static str {
    if schema == "pg_temp" {
        "c.relnamespace = pg_catalog.pg_my_temp_schema() AND c.relname = $2"
    } else {
        "c.relnamespace = (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = $1) AND c.relname = $2"
    }
}

/// Converts one column's values to JSON: numbers, booleans and JSON stay
/// typed, as agate's would in dbt; everything else is its text form.
struct Reader {
    oid: pg_sys::Oid,
    output: pg_sys::Oid,
}

impl Reader {
    fn new(oid: pg_sys::Oid) -> Self {
        let mut output = pg_sys::InvalidOid;
        let mut varlena = false;
        // SAFETY: plain catalog lookup; errors (an unknown type) are
        // raised as Postgres errors.
        unsafe { pg_sys::getTypeOutputInfo(oid, &mut output, &mut varlena) };
        Reader { oid, output }
    }

    fn read(&self, datum: pg_sys::Datum) -> Value {
        // SAFETY: `datum` is a non-null value of type `self.oid`, and
        // `self.output` is that type's output function.
        let text = unsafe {
            let ptr = pg_sys::OidOutputFunctionCall(self.output, datum);
            let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
            pg_sys::pfree(ptr.cast());
            text
        };
        match self.oid {
            pg_sys::BOOLOID => Value::Bool(text == "t"),
            pg_sys::INT2OID | pg_sys::INT4OID | pg_sys::INT8OID | pg_sys::OIDOID => {
                text.parse::<i64>().map_or(Value::String(text), Value::from)
            }
            pg_sys::FLOAT4OID | pg_sys::FLOAT8OID | pg_sys::NUMERICOID => number(text),
            pg_sys::JSONOID | pg_sys::JSONBOID => {
                serde_json::from_str(&text).unwrap_or(Value::String(text))
            }
            _ => Value::String(text),
        }
    }
}

/// Integral values become integers (a `sum()` of integers is numeric in
/// Postgres); NaN and infinities, which JSON cannot hold, stay text.
fn number(text: String) -> Value {
    if let Ok(i) = text.parse::<i64>() {
        return i.into();
    }
    match text.parse::<f64>().ok().and_then(Number::from_f64) {
        Some(n) => Value::Number(n),
        None => Value::String(text),
    }
}

/// Schema-qualified, quoted name of a table owned by an installed extension,
/// resolved through that extension's schema rather than the search_path.
pub fn extension_table(
    client: &SpiClient<'_>,
    extension: &str,
    table: &str,
) -> Result<String, Error> {
    first_text(
        client,
        "SELECT format('%I.%I', n.nspname, $2::text)
           FROM pg_catalog.pg_extension e
           JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
          WHERE e.extname = $1",
        &[extension.into(), table.into()],
    )?
    .ok_or_else(|| Error::new(format!("extension {extension} is not installed")))
}

/// The first column of the first row, if any.
fn first_text(
    client: &SpiClient<'_>,
    sql: &str,
    args: &[DatumWithOid],
) -> Result<Option<String>, Error> {
    let mut rows = client
        .select(sql, Some(1), args)
        .map_err(|e| spi_error(e, sql))?;
    match rows.next() {
        Some(row) => row.get::<String>(1).map_err(|e| spi_error(e, sql)),
        None => Ok(None),
    }
}

fn spi_error(e: SpiError, sql: &str) -> Error {
    Error::new(format!("SPI error: {e}\nwhile running:\n{sql}"))
}
