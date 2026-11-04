//! Statements that build models and seeds.

use crate::context::Relation;
use crate::node::{Materialized, Node};
use crate::seed::{self, Seed};
use crate::session::Session;
use crate::{Column, Dialect, Error};

use super::{Plan, Runner, checksum};

/// Seeds are inserted this many parameters at a time, below SQLite's
/// historical limit of 999 bound parameters per statement.
const MAX_PARAMS: usize = 900;

pub fn model(
    r: &Runner<'_>,
    node: &Node,
    m: Materialized,
    plan: Plan,
) -> Result<(String, Vec<String>), Error> {
    let rel = node.relation.as_ref().expect("built models have relations");
    let d = r.db.dialect();
    let incremental = m == Materialized::Incremental && plan == Plan::Update;
    let sql = r.s.compile(&node.unique_id, incremental)?;
    let mut stmts = Vec::new();
    let action = match (m, plan) {
        (Materialized::Incremental, Plan::Update) => {
            let strategy = incremental_update(r, node, rel, &sql, &mut stmts)?;
            format!("incremental ({strategy})")
        }
        (Materialized::MaterializedView, Plan::Update) => {
            stmts.push(run(
                r,
                format!("REFRESH MATERIALIZED VIEW {}", rel.rendered),
            )?);
            "refresh materialized view".into()
        }
        (Materialized::MaterializedView, _) => {
            if d != Dialect::Postgres {
                return Err(Error::new(format!(
                    "{}: SQLite has no materialized views",
                    node.path
                )));
            }
            stmts.push(run(
                r,
                format!("CREATE MATERIALIZED VIEW {} AS\n{sql}", rel.rendered),
            )?);
            "create materialized view".into()
        }
        (Materialized::View, _) => {
            stmts.push(run(r, format!("CREATE VIEW {} AS\n{sql}", rel.rendered))?);
            "create view".into()
        }
        (Materialized::Table | Materialized::Incremental, _) => {
            stmts.push(run(r, format!("CREATE TABLE {} AS\n{sql}", rel.rendered))?);
            "create table".into()
        }
        (Materialized::Ephemeral, _) => unreachable!("ephemeral models are not built"),
    };
    let kind = if m == Materialized::Incremental {
        "incremental"
    } else {
        m.as_str()
    };
    let sum = (m == Materialized::MaterializedView).then(|| checksum(&sql));
    r.managed
        .record(rel, &node.unique_id, kind, sum.as_deref())?;
    Ok((action, stmts))
}

/// Merges new rows into an existing incremental model and returns the
/// strategy used.
fn incremental_update(
    r: &Runner<'_>,
    node: &Node,
    rel: &Relation,
    sql: &str,
    stmts: &mut Vec<String>,
) -> Result<String, Error> {
    let d = r.db.dialect();
    let tmp = format!("{}__dbt_tmp", rel.identifier);
    stmts.push(run(r, d.create_temp_table_as(&tmp, sql))?);
    let new_cols = r.db.columns(d.temp_schema(), &tmp)?;
    let cols = reconcile_columns(r, node, rel, &new_cols, stmts)?;
    let list = cols
        .iter()
        .map(|c| d.quote(c))
        .collect::<Vec<_>>()
        .join(", ");
    let tmp_q = d.quote(&tmp);
    let keys = node.config.strings("unique_key")?;
    let default = if keys.is_empty() {
        "append"
    } else {
        "delete+insert"
    };
    let strategy = node
        .config
        .str("incremental_strategy")?
        .unwrap_or(default)
        .to_owned();
    let insert = format!(
        "INSERT INTO {} ({list})\nSELECT {list} FROM {tmp_q}",
        rel.rendered
    );
    match strategy.as_str() {
        "append" => stmts.push(run(r, insert)?),
        "delete+insert" => {
            if keys.is_empty() {
                return Err(Error::new(format!(
                    "{}: delete+insert needs a unique_key",
                    node.path
                )));
            }
            let key_list = keys.join(", ");
            let lhs = if keys.len() == 1 {
                key_list.clone()
            } else {
                format!("({key_list})")
            };
            stmts.push(run(
                r,
                format!(
                    "DELETE FROM {} WHERE {lhs} IN (SELECT {key_list} FROM {tmp_q})",
                    rel.rendered
                ),
            )?);
            stmts.push(run(r, insert)?);
        }
        "merge" => {
            if d != Dialect::Postgres {
                return Err(Error::new(format!(
                    "{}: SQLite has no MERGE; use delete+insert",
                    node.path
                )));
            }
            if keys.is_empty() {
                return Err(Error::new(format!(
                    "{}: merge needs a unique_key",
                    node.path
                )));
            }
            let on = keys
                .iter()
                .map(|k| format!("t.{k} = s.{k}"))
                .collect::<Vec<_>>()
                .join(" AND ");
            let set = cols
                .iter()
                .map(|c| format!("{} = s.{}", d.quote(c), d.quote(c)))
                .collect::<Vec<_>>()
                .join(", ");
            let values = cols
                .iter()
                .map(|c| format!("s.{}", d.quote(c)))
                .collect::<Vec<_>>()
                .join(", ");
            stmts.push(run(
                r,
                format!(
                    "MERGE INTO {} AS t USING {tmp_q} AS s ON {on}\n\
                     WHEN MATCHED THEN UPDATE SET {set}\n\
                     WHEN NOT MATCHED THEN INSERT ({list}) VALUES ({values})",
                    rel.rendered
                ),
            )?);
        }
        other => {
            return Err(Error::new(format!(
                "{}: incremental_strategy {other:?} is not supported (append, delete+insert, merge)",
                node.path
            )));
        }
    }
    stmts.push(run(r, format!("DROP TABLE {tmp_q}"))?);
    Ok(strategy)
}

/// Applies `on_schema_change` and returns the columns to insert.
fn reconcile_columns(
    r: &Runner<'_>,
    node: &Node,
    rel: &Relation,
    new_cols: &[Column],
    stmts: &mut Vec<String>,
) -> Result<Vec<String>, Error> {
    let d = r.db.dialect();
    let old_cols = r.db.columns(&rel.schema, &rel.identifier)?;
    let has = |cols: &[Column], name: &str| cols.iter().any(|c| c.name == name);
    let added: Vec<&Column> = new_cols
        .iter()
        .filter(|c| !has(&old_cols, &c.name))
        .collect();
    let removed: Vec<&Column> = old_cols
        .iter()
        .filter(|c| !has(new_cols, &c.name))
        .collect();
    let mode = node.config.str("on_schema_change")?.unwrap_or("ignore");
    match mode {
        "ignore" => {}
        "fail" if added.is_empty() && removed.is_empty() => {}
        "fail" => {
            return Err(Error::new(format!(
                "{}: the model's columns changed (on_schema_change=fail): added {:?}, removed {:?}",
                node.path,
                added.iter().map(|c| &c.name).collect::<Vec<_>>(),
                removed.iter().map(|c| &c.name).collect::<Vec<_>>()
            )));
        }
        "append_new_columns" | "sync_all_columns" => {
            for c in &added {
                stmts.push(run(
                    r,
                    format!(
                        "ALTER TABLE {} ADD COLUMN {} {}",
                        rel.rendered,
                        d.quote(&c.name),
                        column_type(c)
                    ),
                )?);
            }
            if mode == "sync_all_columns" {
                for c in &removed {
                    stmts.push(run(
                        r,
                        format!(
                            "ALTER TABLE {} DROP COLUMN {}",
                            rel.rendered,
                            d.quote(&c.name)
                        ),
                    )?);
                }
            }
            return Ok(new_cols.iter().map(|c| c.name.clone()).collect());
        }
        other => {
            return Err(Error::new(format!(
                "{}: on_schema_change {other:?} is not one of ignore, fail, append_new_columns, sync_all_columns",
                node.path
            )));
        }
    }
    // Only the columns both sides have; the target's order.
    Ok(old_cols
        .into_iter()
        .filter(|c| has(new_cols, &c.name))
        .map(|c| c.name)
        .collect())
}

/// A column's type for ALTER TABLE; SQLite may report none.
fn column_type(c: &Column) -> &str {
    if c.data_type.is_empty() {
        "TEXT"
    } else {
        &c.data_type
    }
}

pub fn seed_data(s: &Session, node: &Node) -> Result<Seed, Error> {
    let text = &s.project.files[&node.path];
    seed::parse(&node.path, text, s.shared.dialect, &node.config)
}

pub fn seed(r: &Runner<'_>, node: &Node, plan: Plan) -> Result<(String, Vec<String>), Error> {
    let rel = node.relation.as_ref().expect("seeds have relations");
    let d = r.db.dialect();
    let data = seed_data(r.s, node)?;
    let mut stmts = Vec::new();
    let action = if plan == Plan::Update {
        stmts.push(run(r, format!("DELETE FROM {}", rel.rendered))?);
        "reload seed"
    } else {
        let cols = data
            .columns
            .iter()
            .map(|(n, t)| format!("{} {t}", d.quote(n)))
            .collect::<Vec<_>>()
            .join(", ");
        stmts.push(run(r, format!("CREATE TABLE {} ({cols})", rel.rendered))?);
        "create seed"
    };
    let width = data.columns.len();
    let list = data
        .columns
        .iter()
        .map(|(n, _)| d.quote(n))
        .collect::<Vec<_>>()
        .join(", ");
    let per_batch = (MAX_PARAMS / width).max(1);
    for batch in data.rows.chunks(per_batch) {
        let mut params: Vec<Option<&str>> = Vec::with_capacity(batch.len() * width);
        let tuples: Vec<String> = batch
            .iter()
            .map(|row| {
                let cells: Vec<String> = data
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, (_, ty))| {
                        params.push(row.get(i).and_then(|v| v.as_deref()));
                        format!("CAST({} AS {ty})", r.db.param(params.len()))
                    })
                    .collect();
                format!("({})", cells.join(", "))
            })
            .collect();
        let sql = format!(
            "INSERT INTO {} ({list}) VALUES\n{}",
            rel.rendered,
            tuples.join(",\n")
        );
        r.db.execute(&sql, &params)
            .map_err(|e| Error::new(format!("{e}\nwhile loading {}", node.path)))?;
    }
    stmts.push(format!(
        "-- {} rows inserted into {}",
        data.rows.len(),
        rel.rendered
    ));
    r.managed.record(rel, &node.unique_id, "seed", None)?;
    Ok((action.into(), stmts))
}

/// `persist_docs`: descriptions from property files as comments, on
/// PostgreSQL (SQLite has no comments).
pub fn persist_docs(r: &Runner<'_>, node: &Node, rel: &Relation) -> Result<Vec<String>, Error> {
    let d = r.db.dialect();
    let Some(cfg) = node.config.get("persist_docs").and_then(|v| v.as_object()) else {
        return Ok(Vec::new());
    };
    if d != Dialect::Postgres {
        return Ok(Vec::new());
    }
    let mut stmts = Vec::new();
    let on = |k: &str| cfg.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    if on("relation") && !node.description.is_empty() {
        let kind = match node.materialized() {
            Some(Materialized::View) => "VIEW",
            Some(Materialized::MaterializedView) => "MATERIALIZED VIEW",
            _ => "TABLE",
        };
        stmts.push(run(
            r,
            format!(
                "COMMENT ON {kind} {} IS {}",
                rel.rendered,
                d.literal(&node.description)
            ),
        )?);
    }
    if on("columns") {
        let existing = r.db.columns(&rel.schema, &rel.identifier)?;
        for c in node.columns.iter().filter(|c| !c.description.is_empty()) {
            if existing.iter().any(|e| e.name == c.name) {
                stmts.push(run(
                    r,
                    format!(
                        "COMMENT ON COLUMN {}.{} IS {}",
                        rel.rendered,
                        d.quote(&c.name),
                        d.literal(&c.description)
                    ),
                )?);
            }
        }
    }
    Ok(stmts)
}

pub fn run(r: &Runner<'_>, sql: String) -> Result<String, Error> {
    r.exec(&sql)?;
    Ok(sql)
}
