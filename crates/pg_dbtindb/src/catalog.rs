//! Reading the project and the state of the target schema.

use dbtindb_core::{Error, Existing, ExistingKind, Project};
use pgrx::datum::DatumWithOid;
use pgrx::spi::{SpiClient, SpiHeapTupleData};

/// The longest identifier Postgres keeps (NAMEDATALEN - 1). Longer names are
/// silently truncated, which could make two models build the same relation.
const MAX_IDENTIFIER_BYTES: usize = 63;

/// Schema-qualified, quoted name of a table owned by an installed extension,
/// resolved through that extension's schema rather than the search_path.
fn extension_table(client: &SpiClient<'_>, extension: &str, table: &str) -> Result<String, Error> {
    let row = first(
        client,
        "SELECT format('%I.%I', n.nspname, $2::text)
           FROM pg_catalog.pg_extension e
           JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
          WHERE e.extname = $1",
        &[extension.into(), table.into()],
        |row| row.get::<String>(1),
    )?;
    row.flatten()
        .ok_or_else(|| Error::new(format!("extension {extension} is not installed")))
}

/// Loads the project from pgjinja's `jinja_templates`.
pub fn load_project(client: &SpiClient<'_>) -> Result<Project, Error> {
    let table = extension_table(client, "pgjinja", "jinja_templates")?;
    let rows = client
        .select(&format!("SELECT name, source FROM {table}"), None, &[])
        .map_err(spi_error)?;
    let mut files = Vec::new();
    for row in rows {
        let name: Option<String> = row.get(1).map_err(spi_error)?;
        let source: Option<String> = row.get(2).map_err(spi_error)?;
        if let (Some(name), Some(source)) = (name, source) {
            files.push((name, source));
        }
    }
    let project = Project::new(files)?;
    if let Some(m) = project
        .models
        .values()
        .find(|m| m.name.len() > MAX_IDENTIFIER_BYTES)
    {
        return Err(Error::new(format!(
            "model name {:?} ({}) is longer than PostgreSQL's {MAX_IDENTIFIER_BYTES}-byte identifier limit",
            m.name, m.path
        )));
    }
    Ok(project)
}

/// The quoted name of this extension's `dbtindb_managed` table.
pub fn managed_table(client: &SpiClient<'_>) -> Result<String, Error> {
    extension_table(client, "pg_dbtindb", "dbtindb_managed")
}

/// What exists at `schema.name`, and whether dbtindb created it.
pub fn existing(
    client: &SpiClient<'_>,
    managed_table: &str,
    schema: &str,
    name: &str,
) -> Result<Option<Existing>, Error> {
    let relkind = first(
        client,
        "SELECT c.relkind::text
           FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = $1 AND c.relname = $2",
        &[schema.into(), name.into()],
        |row| row.get::<String>(1),
    )?
    .flatten();
    let Some(relkind) = relkind else {
        return Ok(None);
    };
    let kind = match relkind.as_str() {
        "v" => ExistingKind::View,
        "r" => ExistingKind::Table,
        "p" => ExistingKind::Other("partitioned table".into()),
        "m" => ExistingKind::Other("materialized view".into()),
        "f" => ExistingKind::Other("foreign table".into()),
        "S" => ExistingKind::Other("sequence".into()),
        "i" | "I" => ExistingKind::Other("index".into()),
        other => ExistingKind::Other(format!("relation of kind {other:?}")),
    };
    let managed = first(
        client,
        &format!("SELECT true FROM {managed_table} WHERE schema_name = $1 AND relation_name = $2"),
        &[schema.into(), name.into()],
        |row| row.get::<bool>(1),
    )?
    .is_some();
    Ok(Some(Existing { kind, managed }))
}

/// Runs a query and maps its first row, if any.
fn first<T>(
    client: &SpiClient<'_>,
    sql: &str,
    args: &[DatumWithOid],
    map: impl FnOnce(SpiHeapTupleData) -> pgrx::spi::SpiResult<T>,
) -> Result<Option<T>, Error> {
    let mut rows = client.select(sql, Some(1), args).map_err(spi_error)?;
    rows.next().map(map).transpose().map_err(spi_error)
}

pub fn spi_error(e: pgrx::spi::SpiError) -> Error {
    Error::new(format!("SPI error: {e}"))
}
