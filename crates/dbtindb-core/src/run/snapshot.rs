//! Snapshots: type 2 slowly changing dimensions, with dbt's `timestamp`
//! and `check` strategies and its `dbt_scd_id`, `dbt_updated_at`,
//! `dbt_valid_from` and `dbt_valid_to` columns.

use crate::node::Node;
use crate::{Dialect, Error};

use super::materialize::run;
use super::{Plan, Runner};

const META: [&str; 4] = [
    "dbt_scd_id",
    "dbt_updated_at",
    "dbt_valid_from",
    "dbt_valid_to",
];

struct Strategy {
    keys: Vec<String>,
    /// When a version starts: the row's `updated_at`, or now.
    updated: Box<dyn Fn(&str) -> String>,
    /// Whether source row `s` changed from current version `t`.
    changed: String,
    invalidate_deletes: bool,
}

pub fn build(r: &Runner<'_>, node: &Node, plan: Plan) -> Result<(String, Vec<String>), Error> {
    let rel = node.relation.as_ref().expect("snapshots have relations");
    let d = r.db.dialect();
    let sql = r.s.compile(&node.unique_id, false)?;
    let mut stmts = Vec::new();
    let tmp = format!("{}__dbt_tmp", rel.identifier);
    let tmp_q = d.quote(&tmp);
    stmts.push(run(r, d.create_temp_table_as(&tmp, &sql))?);
    let source_cols: Vec<String> =
        r.db.columns(d.temp_schema(), &tmp)?
            .into_iter()
            .map(|c| c.name)
            .collect();
    let st = strategy(node, d, &source_cols)?;
    let scd_id = |alias: &str| {
        let mut parts: Vec<String> = st
            .keys
            .iter()
            .map(|k| d.text_or_empty(&format!("{alias}.{k}")))
            .collect();
        parts.push(d.text_or_empty(&(st.updated)(alias)));
        d.md5(&parts.join(" || '|' || "))
    };

    let action = if plan == Plan::Update {
        // Columns the source gained since the last run.
        let existing = r.db.columns(&rel.schema, &rel.identifier)?;
        for c in r.db.columns(d.temp_schema(), &tmp)? {
            if !existing.iter().any(|e| e.name == c.name) {
                let ty = if c.data_type.is_empty() {
                    "TEXT".to_owned()
                } else {
                    c.data_type
                };
                stmts.push(run(
                    r,
                    format!(
                        "ALTER TABLE {} ADD COLUMN {} {ty}",
                        rel.rendered,
                        d.quote(&c.name)
                    ),
                )?);
            }
        }
        let key_match = st
            .keys
            .iter()
            .map(|k| format!("s.{k} = t.{k}"))
            .collect::<Vec<_>>()
            .join(" AND ");
        // Close the current version of every changed row ...
        stmts.push(run(
            r,
            format!(
                "UPDATE {rel} AS t SET dbt_valid_to = (SELECT {upd} FROM {tmp_q} AS s WHERE {key_match})\n\
                 WHERE t.dbt_valid_to IS NULL\n  AND EXISTS (SELECT 1 FROM {tmp_q} AS s WHERE {key_match} AND ({changed}))",
                rel = rel.rendered,
                upd = (st.updated)("s"),
                changed = st.changed,
            ),
        )?);
        // ... and of rows that disappeared from the source ...
        if st.invalidate_deletes {
            stmts.push(run(
                r,
                format!(
                    "UPDATE {rel} AS t SET dbt_valid_to = {now}\n\
                     WHERE t.dbt_valid_to IS NULL\n  AND NOT EXISTS (SELECT 1 FROM {tmp_q} AS s WHERE {key_match})",
                    rel = rel.rendered,
                    now = d.now(),
                ),
            )?);
        }
        // ... then insert a new version for every row without a current one.
        let list = source_cols
            .iter()
            .map(|c| d.quote(c))
            .collect::<Vec<_>>()
            .join(", ");
        let s_list = source_cols
            .iter()
            .map(|c| format!("s.{}", d.quote(c)))
            .collect::<Vec<_>>()
            .join(", ");
        stmts.push(run(
            r,
            format!(
                "INSERT INTO {rel} ({list}, dbt_scd_id, dbt_updated_at, dbt_valid_from, dbt_valid_to)\n\
                 SELECT {s_list}, {id}, {upd}, {upd}, NULL\nFROM {tmp_q} AS s\n\
                 WHERE NOT EXISTS (SELECT 1 FROM {rel} AS t WHERE {key_match} AND t.dbt_valid_to IS NULL)",
                rel = rel.rendered,
                id = scd_id("s"),
                upd = (st.updated)("s"),
            ),
        )?);
        "update snapshot"
    } else {
        let upd = (st.updated)("s");
        stmts.push(run(
            r,
            format!(
                "CREATE TABLE {} AS\nSELECT s.*, {id} AS dbt_scd_id, {upd} AS dbt_updated_at, \
                 {upd} AS dbt_valid_from, nullif({upd}, {upd}) AS dbt_valid_to\nFROM {tmp_q} AS s",
                rel.rendered,
                id = scd_id("s"),
            ),
        )?);
        "create snapshot"
    };
    stmts.push(run(r, format!("DROP TABLE {tmp_q}"))?);
    r.managed.record(rel, &node.unique_id, "snapshot", None)?;
    Ok((action.into(), stmts))
}

fn strategy(node: &Node, d: Dialect, source_cols: &[String]) -> Result<Strategy, Error> {
    let c = &node.config;
    let err = |msg: &str| Error::new(format!("snapshot {}: {msg}", node.name));
    let keys = c.strings("unique_key")?;
    if keys.is_empty() {
        return Err(err("unique_key is required"));
    }
    if let Some(bad) = source_cols.iter().find(|c| META.contains(&c.as_str())) {
        return Err(err(&format!(
            "the query must not return a column named {bad}"
        )));
    }
    let invalidate_deletes = c.bool("invalidate_hard_deletes")?.unwrap_or(false)
        || match c.str("hard_deletes")? {
            None | Some("ignore") => false,
            Some("invalidate") => true,
            Some(other) => {
                return Err(err(&format!(
                    "hard_deletes {other:?} is not supported (ignore, invalidate)"
                )));
            }
        };
    match c.str("strategy")? {
        Some("timestamp") => {
            let col = c
                .str("updated_at")?
                .ok_or_else(|| err("the timestamp strategy needs updated_at"))?
                .to_owned();
            let changed = format!("s.{col} > t.dbt_updated_at");
            Ok(Strategy {
                keys,
                updated: Box::new(move |a| format!("{a}.{col}")),
                changed,
                invalidate_deletes,
            })
        }
        Some("check") => {
            let cols: Vec<String> = match c.get("check_cols") {
                Some(serde_json::Value::String(s)) if s == "all" => source_cols
                    .iter()
                    .filter(|col| !keys.contains(col))
                    .map(|col| d.quote(col))
                    .collect(),
                Some(_) => c.strings("check_cols")?,
                None => return Err(err("the check strategy needs check_cols")),
            };
            if cols.is_empty() {
                return Err(err("check_cols is empty"));
            }
            let changed = cols
                .iter()
                .map(|col| d.is_distinct(&format!("s.{col}"), &format!("t.{col}")))
                .collect::<Vec<_>>()
                .join(" OR ");
            let now = d.now().to_owned();
            Ok(Strategy {
                keys,
                updated: Box::new(move |_| now.clone()),
                changed,
                invalidate_deletes,
            })
        }
        Some(other) => Err(err(&format!(
            "strategy {other:?} is not supported (timestamp, check)"
        ))),
        None => Err(err("strategy is required (timestamp or check)")),
    }
}
