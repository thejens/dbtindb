//! Compiling nodes for execution: rendering with `execute` true and the
//! node's final config, then inlining the ephemeral models it refs.

use std::collections::BTreeSet;

use crate::node::{Node, NodeKind};
use crate::session::Session;
use crate::{Error, testgen};

impl Session {
    /// The SQL a node compiles to: a model's or snapshot's SELECT, or a
    /// test's query for failing rows. `is_incremental` is what the node's
    /// `is_incremental()` returns.
    pub fn compile(&self, unique_id: &str, is_incremental: bool) -> Result<String, Error> {
        let mut seen = BTreeSet::new();
        let (sql, ctes) = self.render_node(unique_id, is_incremental)?;
        let mut defs = Vec::new();
        for cte in ctes {
            self.collect_cte(&cte, &mut seen, &mut defs)?;
        }
        Ok(inject_ctes(&sql, &defs))
    }

    /// Ad-hoc SQL rendered in the project's context, as `dbt compile
    /// --inline` does: `ref` and `source` resolve to the project's
    /// relations, ephemeral models become CTEs, and `execute` is true, so
    /// `run_query` works. There is no `this`, since it is no node.
    pub fn compile_inline(&self, sql: &str) -> Result<String, Error> {
        self.shared.begin(
            true,
            "inline SQL",
            None,
            Default::default(),
            Default::default(),
        );
        let rendered = self.render_str("<inline>", sql);
        let cur = self.shared.finish();
        let rendered = rendered.map_err(|e| Error::new(format!("compiling inline SQL: {e}")))?;
        let mut seen = BTreeSet::new();
        let mut defs = Vec::new();
        for cte in cur.ctes {
            self.collect_cte(&cte, &mut seen, &mut defs)?;
        }
        Ok(inject_ctes(&trim_statement(&rendered), &defs))
    }

    /// Renders a hook (or any SQL template) with `node` as the context.
    pub fn render_hook(&self, unique_id: Option<&str>, hook: &str) -> Result<String, Error> {
        match unique_id {
            Some(id) => {
                self.begin_execute(&self.manifest.nodes[id], false);
                let out = self.render_str("<hook>", hook);
                self.shared.finish();
                out
            }
            None => {
                self.shared.begin(
                    true,
                    "on-run hook",
                    None,
                    Default::default(),
                    Default::default(),
                );
                let out = self.render_str("<hook>", hook);
                self.shared.finish();
                out
            }
        }
        .map(|s| s.trim().to_owned())
    }

    fn begin_execute(&self, node: &Node, is_incremental: bool) {
        self.shared.begin(
            true,
            &node.unique_id,
            node.relation.clone(),
            self.node_json(&node.unique_id),
            node.config.clone(),
        );
        let mut cur = self.shared.current.lock().unwrap();
        cur.is_incremental = is_incremental;
        cur.parsed_refs = Some(node.depends_on.clone());
    }

    /// Renders a node, returning its SQL and the ephemeral models it refs.
    fn render_node(
        &self,
        unique_id: &str,
        is_incremental: bool,
    ) -> Result<(String, Vec<String>), Error> {
        let node = self
            .manifest
            .nodes
            .get(unique_id)
            .ok_or_else(|| Error::new(format!("no enabled node {unique_id}")))?;
        self.begin_execute(node, is_incremental);
        let rendered = match &node.kind {
            NodeKind::Model(_) | NodeKind::SingularTest | NodeKind::Analysis => {
                self.render_file(&node.path)
            }
            NodeKind::Snapshot => self.call_macro(&format!("snapshot_{}", node.name), &[]),
            NodeKind::GenericTest { target, .. } => {
                testgen::tested_relation(self, &self.manifest.nodes, target)
                    .and_then(|rel| testgen::call_test(self, node, rel))
            }
            NodeKind::Seed => Err(Error::new(format!("{unique_id} is a seed; it has no SQL"))),
        };
        let cur = self.shared.finish();
        let rendered = rendered.map_err(|e| Error::new(format!("compiling {}: {e}", node.path)))?;
        Ok((trim_statement(&rendered), cur.ctes))
    }

    /// Adds `unique_id` (an ephemeral model) and the ephemerals it refs to
    /// `defs`, dependencies first.
    fn collect_cte(
        &self,
        unique_id: &str,
        seen: &mut BTreeSet<String>,
        defs: &mut Vec<(String, String)>,
    ) -> Result<(), Error> {
        if !seen.insert(unique_id.to_owned()) {
            return Ok(());
        }
        let (sql, ctes) = self.render_node(unique_id, false)?;
        for cte in ctes {
            self.collect_cte(&cte, seen, defs)?;
        }
        let rel = self.manifest.nodes[unique_id]
            .relation
            .as_ref()
            .expect("ephemerals have a CTE name");
        defs.push((rel.rendered.clone(), sql));
        Ok(())
    }
}

/// Puts ephemeral models in front of `sql` as CTEs, merging with a WITH
/// clause the query already has, as dbt does.
fn inject_ctes(sql: &str, defs: &[(String, String)]) -> String {
    if defs.is_empty() {
        return sql.to_owned();
    }
    let ctes: Vec<String> = defs
        .iter()
        .map(|(name, body)| format!("{name} as (\n{body}\n)"))
        .collect();
    let ctes = ctes.join(",\n");
    match leading_with(sql) {
        Some(after_with) => format!(
            "{}\n{ctes},\n{}",
            &sql[..after_with],
            sql[after_with..].trim_start()
        ),
        None => format!("with {ctes}\n{sql}"),
    }
}

/// If `sql` starts with `WITH` (after whitespace and comments), the byte
/// offset just after it, and after `RECURSIVE` if present.
fn leading_with(sql: &str) -> Option<usize> {
    let mut i = 0;
    let bytes = sql.as_bytes();
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if sql[i..].starts_with("--") {
            i += sql[i..].find('\n').map_or(sql.len() - i, |n| n + 1);
        } else if sql[i..].starts_with("/*") {
            i += sql[i..].find("*/").map_or(sql.len() - i, |n| n + 2);
        } else {
            break;
        }
    }
    let word = |at: usize, w: &str| {
        sql.get(at..at + w.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(w))
            && bytes
                .get(at + w.len())
                .is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_')
    };
    if !word(i, "with") {
        return None;
    }
    let mut end = i + 4;
    let mut j = end;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if word(j, "recursive") {
        end = j + "recursive".len();
    }
    Some(end)
}

/// Trims whitespace and one trailing semicolon, so the SELECT can be
/// wrapped in CREATE ... AS or a subquery.
pub fn trim_statement(sql: &str) -> String {
    let sql = sql.trim();
    sql.strip_suffix(';').unwrap_or(sql).trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use crate::Dialect;
    use crate::parse_tests::load;

    #[test]
    fn compiles_inline_sql_against_the_project() {
        let s = load(
            &[
                (
                    "models/base.sql",
                    "{{ config(materialized='ephemeral') }} select 1 as id",
                ),
                (
                    "models/orders.sql",
                    "{{ config(schema='marts') }} select 1 as id",
                ),
                (
                    "macros/m.sql",
                    "{% macro double(x) %}{{ x }} * 2{% endmacro %}",
                ),
            ],
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            s.compile_inline("select {{ double('o.id') }} from {{ ref('orders') }} o join {{ ref('base') }} b using (id);\n")
                .unwrap(),
            "with \"__dbt__cte__base\" as (\nselect 1 as id\n)\nselect o.id * 2 from \"analytics_marts\".\"orders\" o join \"__dbt__cte__base\" b using (id)"
        );
        let err = s
            .compile_inline("select * from {{ ref('nope') }}")
            .unwrap_err();
        assert!(err.0.contains("nope"), "{err}");
    }

    #[test]
    fn inlines_ephemeral_models_as_ctes() {
        let s = load(
            &[
                (
                    "models/base.sql",
                    "{{ config(materialized='ephemeral') }} select 1 as id",
                ),
                (
                    "models/mid.sql",
                    "{{ config(materialized='ephemeral') }} select * from {{ ref('base') }}",
                ),
                (
                    "models/top.sql",
                    "-- top\nWITH x AS (select * from {{ ref('mid') }}) select * from x;",
                ),
                ("models/plain.sql", "select * from {{ ref('base') }}"),
            ],
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            s.compile("model.dbtindb.top", false).unwrap(),
            "-- top\nWITH\n\"__dbt__cte__base\" as (\nselect 1 as id\n),\n\"__dbt__cte__mid\" as (\nselect * from \"__dbt__cte__base\"\n),\nx AS (select * from \"__dbt__cte__mid\") select * from x"
        );
        assert_eq!(
            s.compile("model.dbtindb.plain", false).unwrap(),
            "with \"__dbt__cte__base\" as (\nselect 1 as id\n)\nselect * from \"__dbt__cte__base\""
        );
    }

    #[test]
    fn execute_sees_final_config_and_is_incremental() {
        let s = load(
            &[(
                "models/m.sql",
                "{{ config(materialized='incremental', unique_key='id') }}select {{ config.get('unique_key') }}{% if is_incremental() %} from {{ this }}{% endif %}{% if execute %} -- run{% endif %}",
            )],
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            s.compile("model.dbtindb.m", false).unwrap(),
            "select id -- run"
        );
        assert_eq!(
            s.compile("model.dbtindb.m", true).unwrap(),
            "select id from \"analytics\".\"m\" -- run"
        );
    }

    #[test]
    fn generic_tests_compile_with_where() {
        let s = load(
            &[
                ("models/m.sql", "select 1 as id"),
                ("models/s.yml", "models:\n  - name: m\n    columns:\n      - name: id\n        tests:\n          - accepted_values:\n              values: [1, 2]\n              quote: false\n              config: {where: \"id > 0\"}\n"),
            ],
            Dialect::Postgres,
        )
        .unwrap();
        let sql = s
            .compile("test.dbtindb.accepted_values_m_id", false)
            .unwrap();
        assert!(
            sql.contains("from (select * from \"analytics\".\"m\" where id > 0) dbt_subquery"),
            "{sql}"
        );
        assert!(sql.contains("not in (1, 2)"), "{sql}");
    }
}
