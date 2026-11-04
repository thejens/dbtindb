//! The SQL-callable functions.

use dbinja_core::{build_order, compile, plan, CompiledModel, Dialect, Error, Statement, Target};
use pgrx::prelude::*;
use pgrx::spi::SpiClient;
use pgrx::JsonB;

use crate::catalog::{self, spi_error};

/// One executed statement: (step, model, action, statement).
type RunRow = (i32, String, String, String);

/// Builds every enabled model into `target_schema`, in dependency order,
/// and returns the statements it ran.
///
/// Runs in the caller's transaction, so a failure anywhere leaves
/// everything as it was. Existing model relations are dropped and
/// recreated; relations dbinja did not create are never touched, and
/// nothing is dropped with CASCADE.
#[pg_extern(volatile, parallel_unsafe)]
fn dbinja_run(
    target_schema: default!(&str, "'public'"),
    vars: default!(JsonB, "'{}'::jsonb"),
) -> TableIterator<
    'static,
    (
        name!(step, i32),
        name!(model, String),
        name!(action, String),
        name!(statement, String),
    ),
> {
    let rows = Spi::connect_mut(|client| run(client, target_schema, &vars.0));
    TableIterator::new(rows.unwrap_or_else(|e| error!("dbinja: {e}")))
}

fn run(
    client: &mut SpiClient<'_>,
    schema: &str,
    vars: &serde_json::Value,
) -> Result<Vec<RunRow>, Error> {
    let project = catalog::load_project(client)?;
    let models = compile(&project, &target(schema), vars)?;
    let order = build_order(&models)?;
    let managed = catalog::managed_table(client)?;
    let statements = plan(&models, &order, |m| {
        catalog::existing(client, &managed, schema, &m.name)
    })?;

    let quoted = Dialect::Postgres.quote(schema);
    client
        .update(&format!("CREATE SCHEMA IF NOT EXISTS {quoted}"), None, &[])
        .map_err(spi_error)?;

    let mut rows = Vec::with_capacity(statements.len());
    for (step, statement) in statements.iter().enumerate() {
        client
            .update(statement.sql(), None, &[])
            .map_err(|e| Error::new(format!("{e}\nwhile running:\n{}", statement.sql())))?;
        // The bookkeeping row moves in the same transaction as the DDL, so
        // dbinja_managed always matches what actually exists.
        let (model, action) = match statement {
            Statement::Drop { model, .. } => {
                client
                    .update(
                        &format!(
                            "DELETE FROM {managed} WHERE schema_name = $1 AND relation_name = $2"
                        ),
                        None,
                        &[schema.into(), model.as_str().into()],
                    )
                    .map_err(spi_error)?;
                (model, "drop")
            }
            Statement::Create { model, .. } => {
                let compiled = find(&models, model);
                client
                    .update(
                        &format!(
                            "INSERT INTO {managed} (schema_name, relation_name, path, materialized) \
                             VALUES ($1, $2, $3, $4)"
                        ),
                        None,
                        &[
                            schema.into(),
                            model.as_str().into(),
                            compiled.path.as_str().into(),
                            compiled.materialized.as_str().into(),
                        ],
                    )
                    .map_err(spi_error)?;
                (model, "create")
            }
        };
        rows.push((
            i32::try_from(step + 1).expect("fewer than 2^31 statements"),
            model.clone(),
            action.to_owned(),
            statement.sql().to_owned(),
        ));
    }
    Ok(rows)
}

/// Returns `model` compiled to plain SQL, as `dbt compile` would.
#[pg_extern(stable, parallel_safe)]
fn dbinja_compile(
    model: &str,
    target_schema: default!(&str, "'public'"),
    vars: default!(JsonB, "'{}'::jsonb"),
) -> String {
    let result = Spi::connect(|client| -> Result<String, Error> {
        let project = catalog::load_project(client)?;
        compile(&project, &target(target_schema), &vars.0)?
            .into_iter()
            .find(|m| m.name == model)
            .map(|m| m.sql)
            .ok_or_else(|| Error::new(format!("no enabled model named {model:?}")))
    });
    result.unwrap_or_else(|e| error!("dbinja: {e}"))
}

/// (build_order, model, path, materialized, relation, depends_on, sources)
type ModelRow = (
    i32,
    String,
    String,
    String,
    String,
    Vec<String>,
    Vec<String>,
);

/// Lists the enabled models in build order, with their lineage.
// pgrx reads the column names from the signature, so the row type cannot
// be a type alias.
#[allow(clippy::type_complexity)]
#[pg_extern(stable, parallel_safe)]
fn dbinja_models(
    target_schema: default!(&str, "'public'"),
    vars: default!(JsonB, "'{}'::jsonb"),
) -> TableIterator<
    'static,
    (
        name!(build_order, i32),
        name!(model, String),
        name!(path, String),
        name!(materialized, String),
        name!(relation, String),
        name!(depends_on, Vec<String>),
        name!(sources, Vec<String>),
    ),
> {
    let result = Spi::connect(|client| -> Result<Vec<ModelRow>, Error> {
        let project = catalog::load_project(client)?;
        let models = compile(&project, &target(target_schema), &vars.0)?;
        let order = build_order(&models)?;
        Ok(order
            .iter()
            .enumerate()
            .map(|(pos, &i)| {
                let m = &models[i];
                (
                    i32::try_from(pos + 1).expect("fewer than 2^31 models"),
                    m.name.clone(),
                    m.path.clone(),
                    m.materialized.as_str().to_owned(),
                    m.relation.clone(),
                    m.depends_on.iter().cloned().collect(),
                    m.sources.iter().cloned().collect(),
                )
            })
            .collect())
    });
    TableIterator::new(result.unwrap_or_else(|e| error!("dbinja: {e}")))
}

fn target(schema: &str) -> Target {
    Target {
        dialect: Dialect::Postgres,
        schema: schema.to_owned(),
    }
}

fn find<'a>(models: &'a [CompiledModel], name: &str) -> &'a CompiledModel {
    models
        .iter()
        .find(|m| m.name == name)
        .expect("planned statements only name compiled models")
}
