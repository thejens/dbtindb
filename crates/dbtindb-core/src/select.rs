//! dbt's node selection syntax, for `select` and `exclude`.
//!
//! Space-separated terms are unioned, comma-separated ones intersected.
//! Each term is `[@][n+]method:value[+n]`; without a method, the value
//! matches node names and folders (dbt's `fqn` method) or, if it looks like
//! a path, file paths. Methods: `tag`, `path`, `config.<key>`,
//! `resource_type`, `source`, `test_type`, `test_name`, `fqn`, `package`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::Error;
use crate::graph::reachable;
use crate::node::{Manifest, Node, NodeKind};

/// Applies `select` and `exclude` to the manifest. An empty `select`
/// selects everything. Tests are selected when any node or source they
/// test is (dbt's "eager" indirect selection), unless excluded.
pub fn select(m: &Manifest, select: &str, exclude: &str) -> Result<BTreeSet<String>, Error> {
    let graph = Graph::new(m);
    let mut chosen: BTreeSet<String> = if select.trim().is_empty() {
        graph.all.clone()
    } else {
        graph.eval(m, select)?
    };
    // Tests follow what they test.
    for (id, node) in &m.nodes {
        if node.is_test() && graph.parents_of(id).iter().any(|p| chosen.contains(p)) {
            chosen.insert(id.clone());
        }
    }
    if !exclude.trim().is_empty() {
        let excluded = graph.eval(m, exclude)?;
        chosen.retain(|id| !excluded.contains(id));
        // A test of an excluded node goes with it.
        chosen.retain(|id| {
            !m.nodes.get(id).is_some_and(|n| n.is_test())
                || !graph
                    .parents_of(id)
                    .iter()
                    .any(|p| excluded.contains(p) && !p.starts_with("source."))
        });
    }
    // Sources are only stepping stones for graph operators.
    chosen.retain(|id| m.nodes.contains_key(id));
    Ok(chosen)
}

/// Nodes and sources, with edges in both directions. Sources are graph
/// members (`source.<source>.<table>`) so that `source:raw+` works.
struct Graph {
    all: BTreeSet<String>,
    parents: BTreeMap<String, Vec<String>>,
    children: BTreeMap<String, Vec<String>>,
    sources: BTreeMap<String, (String, String)>,
}

impl Graph {
    fn new(m: &Manifest) -> Self {
        let mut g = Graph {
            all: m.nodes.keys().cloned().collect(),
            parents: BTreeMap::new(),
            children: BTreeMap::new(),
            sources: BTreeMap::new(),
        };
        for (id, n) in &m.nodes {
            let mut ps: Vec<String> = n.depends_on.iter().cloned().collect();
            for key in &n.sources {
                let sid = format!("source.{}.{}", key.0, key.1);
                g.sources.insert(sid.clone(), key.clone());
                ps.push(sid);
            }
            for p in &ps {
                g.children.entry(p.clone()).or_default().push(id.clone());
            }
            g.parents.insert(id.clone(), ps);
        }
        g
    }

    fn parents_of(&self, id: &str) -> Vec<String> {
        self.parents.get(id).cloned().unwrap_or_default()
    }

    fn children_of(&self, id: &str) -> Vec<String> {
        self.children.get(id).cloned().unwrap_or_default()
    }

    fn eval(&self, m: &Manifest, expr: &str) -> Result<BTreeSet<String>, Error> {
        let mut union = BTreeSet::new();
        for term in expr.split_whitespace() {
            let mut acc: Option<BTreeSet<String>> = None;
            for atom in term.split(',') {
                let set = self.atom(m, atom)?;
                acc = Some(match acc {
                    None => set,
                    Some(prev) => prev.intersection(&set).cloned().collect(),
                });
            }
            union.extend(acc.unwrap_or_default());
        }
        Ok(union)
    }

    fn atom(&self, m: &Manifest, atom: &str) -> Result<BTreeSet<String>, Error> {
        let (at, rest) = match atom.strip_prefix('@') {
            Some(r) => (true, r),
            None => (false, atom),
        };
        let (up, rest) = graph_op(rest, true);
        let (down, core) = graph_op(rest, false);
        if core.is_empty() {
            return Err(Error::new(format!("invalid selector {atom:?}")));
        }
        let base = self.matches(m, core)?;
        let mut out = base.clone();
        for id in &base {
            if let Some(depth) = up {
                out.extend(reachable(id, |i| self.parents_of(i), depth));
            }
            if at {
                // @x: x, everything below it, and everything those need.
                let below = reachable(id, |i| self.children_of(i), None);
                for b in &below {
                    out.extend(reachable(b, |i| self.parents_of(i), None));
                }
                out.extend(below);
            } else if let Some(depth) = down {
                out.extend(reachable(id, |i| self.children_of(i), depth));
            }
        }
        Ok(out)
    }

    fn matches(&self, m: &Manifest, core: &str) -> Result<BTreeSet<String>, Error> {
        let (method, value) = match core.split_once(':') {
            Some((method, value)) => (method, value),
            None if core.contains('/') || core.ends_with(".sql") || core.ends_with(".csv") => {
                ("path", core)
            }
            None => ("fqn", core),
        };
        if method == "source" {
            let (src, table) = value.split_once('.').unwrap_or((value, "*"));
            return Ok(self
                .sources
                .iter()
                .filter(|(_, (s, t))| glob(src, s) && glob(table, t))
                .map(|(id, _)| id.clone())
                .collect());
        }
        let mut out = BTreeSet::new();
        for (id, n) in &m.nodes {
            if node_matches(n, method, value)? {
                out.insert(id.clone());
            }
        }
        Ok(out)
    }
}

/// Splits a leading (`2+x`, `+x`) or trailing (`x+`, `x+3`) graph operator
/// off `s`. `Some(None)` means unlimited depth.
fn graph_op(s: &str, leading: bool) -> (Option<Option<usize>>, &str) {
    if leading {
        let digits = s.chars().take_while(char::is_ascii_digit).count();
        match s[digits..].strip_prefix('+') {
            Some(rest) => (Some(s[..digits].parse().ok()), rest),
            None => (None, s),
        }
    } else {
        let digits = s.chars().rev().take_while(char::is_ascii_digit).count();
        let (head, n) = s.split_at(s.len() - digits);
        match head.strip_suffix('+') {
            Some(rest) => (Some(n.parse().ok()), rest),
            None => (None, s),
        }
    }
}

fn node_matches(n: &Node, method: &str, value: &str) -> Result<bool, Error> {
    Ok(match method {
        "fqn" => {
            let parts: Vec<&str> = value.split('.').collect();
            let fqn: Vec<&str> = n.fqn.iter().map(String::as_str).collect();
            let prefix =
                |f: &[&str]| f.len() >= parts.len() && parts.iter().zip(f).all(|(p, s)| glob(p, s));
            (parts.len() == 1 && glob(value, &n.name)) || prefix(&fqn) || prefix(&fqn[1..])
        }
        "path" | "file" => {
            let v = value.trim_end_matches('/');
            glob(v, &n.path) || n.path.starts_with(&format!("{v}/"))
        }
        "tag" => n.config.tags()?.iter().any(|t| glob(value, t)),
        "resource_type" => n.resource_type.as_str() == value,
        "package" => n.fqn.first().is_some_and(|p| glob(value, p)),
        "test_type" => match value {
            "generic" => matches!(n.kind, NodeKind::GenericTest { .. }),
            "singular" => matches!(n.kind, NodeKind::SingularTest),
            // dbt 1.8's names: data tests are generic and singular ones.
            "data" => matches!(
                n.kind,
                NodeKind::GenericTest { .. } | NodeKind::SingularTest
            ),
            "unit" => matches!(n.kind, NodeKind::UnitTest(_)),
            _ => {
                return Err(Error::new(format!(
                    "test_type:{value} is not generic, singular, data or unit"
                )));
            }
        },
        "test_name" => match &n.kind {
            NodeKind::GenericTest { macro_name, .. } => {
                // `test_unique`, or `<package>.test_unique` for a package's.
                let macro_name = macro_name.rsplit('.').next().unwrap_or(macro_name);
                glob(value, macro_name.trim_start_matches("test_"))
            }
            _ => false,
        },
        m if m.starts_with("config.") => {
            let key = &m["config.".len()..];
            match n.config.get(key) {
                Some(Value::String(s)) => glob(value, s),
                Some(Value::Array(items)) => items
                    .iter()
                    .any(|i| i.as_str().is_some_and(|s| glob(value, s))),
                Some(other) => other == value,
                None => {
                    key == "materialized"
                        && n.materialized().is_some_and(|mat| mat.as_str() == value)
                }
            }
        }
        other => return Err(Error::new(format!("unknown selector method {other:?}"))),
    })
}

/// Shell-style wildcard match: `*` and `?`.
fn glob(pattern: &str, s: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &s[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), s.as_bytes())
}

#[cfg(test)]
mod tests {
    use crate::Dialect;
    use crate::parse_tests::load;

    use super::*;

    fn names(selected: BTreeSet<String>) -> Vec<String> {
        selected
            .into_iter()
            .map(|id| id.rsplit('.').next().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn selects_with_dbt_syntax() {
        let s = load(
            &[
                ("models/staging/stg_a.sql", "{{ config(tags=['nightly']) }} select * from {{ source('raw', 'a') }}"),
                ("models/staging/stg_b.sql", "select 1 as id"),
                ("models/marts/ab.sql", "{{ config(materialized='table') }} select * from {{ ref('stg_a') }}, {{ ref('stg_b') }}"),
                ("models/marts/report.sql", "select * from {{ ref('ab') }}"),
                ("models/s.yml", "models:\n  - name: ab\n    columns:\n      - name: id\n        tests: [not_null]\n"),
            ],
            Dialect::Postgres,
        )
        .unwrap();
        let m = &s.manifest;
        let sel = |s: &str, e: &str| names(select(m, s, e).unwrap());
        assert_eq!(sel("stg_a", ""), ["stg_a"]);
        assert_eq!(
            sel("stg_a+", ""),
            ["ab", "report", "stg_a", "not_null_ab_id"]
        );
        assert_eq!(sel("1+ab", ""), ["ab", "stg_a", "stg_b", "not_null_ab_id"]);
        assert_eq!(sel("marts", "report"), ["ab", "not_null_ab_id"]);
        assert_eq!(sel("tag:nightly", ""), ["stg_a"]);
        assert_eq!(
            sel("source:raw+", ""),
            ["ab", "report", "stg_a", "not_null_ab_id"]
        );
        assert_eq!(sel("models/staging", ""), ["stg_a", "stg_b"]);
        assert_eq!(
            sel("config.materialized:table", ""),
            ["ab", "not_null_ab_id"]
        );
        assert_eq!(sel("staging,tag:nightly stg_b", ""), ["stg_a", "stg_b"]);
        assert_eq!(
            sel("@stg_b", ""),
            ["ab", "report", "stg_a", "stg_b", "not_null_ab_id"]
        );
        assert_eq!(sel("resource_type:test", ""), ["not_null_ab_id"]);
        assert_eq!(sel("", "ab"), ["report", "stg_a", "stg_b"]);
    }
}
