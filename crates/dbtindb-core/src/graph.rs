//! The DAG of nodes: build order, ancestors and descendants.

use std::collections::{BTreeMap, BTreeSet};

use crate::Error;
use crate::node::Manifest;

/// Unique ids in an order where every node comes after its dependencies,
/// and, as in `dbt build`, after the tests of the nodes it builds on: a
/// failing test is reported before anything downstream of it is built.
/// Ties are broken by unique id, so the order is stable.
pub fn build_order(m: &Manifest) -> Result<Vec<String>, Error> {
    let deps: BTreeMap<&str, BTreeSet<&str>> = m
        .nodes
        .iter()
        .map(|(id, n)| {
            (
                id.as_str(),
                n.depends_on.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    // Cycles are found on the declared edges alone, so the error names
    // nodes the user wrote refs between rather than implied test edges.
    if let Err(remaining) = topological(&deps) {
        return Err(Error::new(format!(
            "dependency cycle: {}",
            find_cycle(m, &remaining).join(" -> ")
        )));
    }
    let gated = with_test_edges(m, deps);
    Ok(topological(&gated)
        .expect("test edges only point downstream, so they cannot close a cycle")
        .into_iter()
        .map(str::to_owned)
        .collect())
}

/// Kahn's algorithm. On a cycle, returns the nodes that could not be
/// ordered with their count of unordered dependencies.
fn topological<'a>(
    deps: &BTreeMap<&'a str, BTreeSet<&'a str>>,
) -> Result<Vec<&'a str>, BTreeMap<&'a str, usize>> {
    let mut remaining: BTreeMap<&str, usize> = deps.iter().map(|(&id, d)| (id, d.len())).collect();
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (&id, d) in deps {
        for &dep in d {
            children.entry(dep).or_default().push(id);
        }
    }
    let mut ready: BTreeSet<&str> = remaining
        .iter()
        .filter(|&(_, &n)| n == 0)
        .map(|(&id, _)| id)
        .collect();
    let mut order = Vec::with_capacity(deps.len());
    while let Some(id) = ready.pop_first() {
        remaining.remove(id);
        order.push(id);
        for &c in children.get(id).into_iter().flatten() {
            let n = remaining
                .get_mut(c)
                .expect("a child is pending until its parents are done");
            *n -= 1;
            if *n == 0 {
                ready.insert(c);
            }
        }
    }
    if remaining.is_empty() {
        Ok(order)
    } else {
        Err(remaining)
    }
}

/// Adds an edge from each test to each non-test child of a node or source
/// it tests, when everything the test reads is upstream of that child. The
/// condition is dbt's: a `relationships` test on `orders` that also reads
/// `customers` must not gate `customers` itself, which would be a cycle.
fn with_test_edges<'a>(
    m: &'a Manifest,
    mut deps: BTreeMap<&'a str, BTreeSet<&'a str>>,
) -> BTreeMap<&'a str, BTreeSet<&'a str>> {
    // Sources are not nodes; they take part here as parent-only keys.
    let parents = |id: &str| -> Vec<String> {
        m.nodes.get(id).map_or_else(Vec::new, |n| {
            let sources = n.sources.iter().map(|(s, t)| format!("source.{s}.{t}"));
            n.depends_on.iter().cloned().chain(sources).collect()
        })
    };
    let mut tests_of: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (id, n) in &m.nodes {
        if n.is_test() {
            for p in parents(id) {
                tests_of.entry(p).or_default().push(id.as_str());
            }
        }
    }
    for (id, n) in &m.nodes {
        if n.is_test() {
            continue;
        }
        let upstream = reachable(id, parents, None);
        let gates: BTreeSet<&str> = parents(id)
            .iter()
            .flat_map(|p| tests_of.get(p).into_iter().flatten().copied())
            .filter(|t| parents(t).iter().all(|p| upstream.contains(p)))
            .collect();
        deps.get_mut(id.as_str())
            .expect("every node has an entry")
            .extend(gates);
    }
    deps
}

/// For each node, the nodes that depend on it.
pub fn children(m: &Manifest) -> BTreeMap<&str, Vec<&str>> {
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, n) in &m.nodes {
        for dep in &n.depends_on {
            children.entry(dep.as_str()).or_default().push(id.as_str());
        }
    }
    children
}

/// All ids reachable from `start` along `edges`, up to `depth` steps
/// (unlimited when `None`), not including `start` itself.
pub fn reachable(
    start: &str,
    edges: impl Fn(&str) -> Vec<String>,
    depth: Option<usize>,
) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut frontier = vec![start.to_owned()];
    let mut level = 0;
    while !frontier.is_empty() && depth.is_none_or(|d| level < d) {
        let mut next = Vec::new();
        for id in frontier {
            for e in edges(&id) {
                if seen.insert(e.clone()) {
                    next.push(e);
                }
            }
        }
        frontier = next;
        level += 1;
    }
    seen
}

fn find_cycle<'a>(m: &'a Manifest, remaining: &BTreeMap<&'a str, usize>) -> Vec<&'a str> {
    // Every leftover node has a leftover dependency, so walking those
    // edges from any leftover node must revisit one.
    let name = |id: &'a str| m.nodes[id].name.as_str();
    let mut path: Vec<&str> = Vec::new();
    let mut current = *remaining.keys().next().expect("a cycle has members");
    loop {
        if let Some(pos) = path.iter().position(|&p| p == current) {
            let mut cycle: Vec<&str> = path[pos..].iter().map(|&id| name(id)).collect();
            cycle.push(name(current));
            return cycle;
        }
        path.push(current);
        current = m.nodes[current]
            .depends_on
            .iter()
            .map(String::as_str)
            .find(|d| remaining.contains_key(d))
            .expect("a leftover node has a leftover dependency");
    }
}

#[cfg(test)]
mod tests {
    use crate::Dialect;
    use crate::parse_tests::load;

    use super::*;

    #[test]
    fn orders_dependencies_first() {
        let s = load(
            &[
                (
                    "models/orders.sql",
                    "select * from {{ ref('stg') }} join {{ ref('countries') }} on true",
                ),
                ("models/stg.sql", "select 1 as id"),
                ("seeds/countries.csv", "code\nSE\n"),
            ],
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            build_order(&s.manifest).unwrap(),
            [
                "model.dbtindb.stg",
                "seed.dbtindb.countries",
                "model.dbtindb.orders"
            ]
        );
    }

    #[test]
    fn runs_tests_before_what_builds_on_the_tested_node() {
        let s = load(
            &[
                ("models/stg.sql", "select 1 as id"),
                ("models/orders.sql", "select * from {{ ref('stg') }}"),
                (
                    "models/schema.yml",
                    "models:\n  - name: stg\n    columns:\n      - name: id\n        data_tests:\n          - not_null\n          - relationships: {to: ref('orders'), field: id}\n",
                ),
            ],
            Dialect::Postgres,
        )
        .unwrap();
        // The relationships test reads `orders`, so it cannot gate it.
        assert_eq!(
            build_order(&s.manifest).unwrap(),
            [
                "model.dbtindb.stg",
                "test.dbtindb.not_null_stg_id",
                "model.dbtindb.orders",
                "test.dbtindb.relationships_stg_id",
            ]
        );
    }

    #[test]
    fn names_the_cycle() {
        let err = load(
            &[
                ("models/a.sql", "select * from {{ ref('c') }}"),
                ("models/b.sql", "select * from {{ ref('a') }}"),
                ("models/c.sql", "select * from {{ ref('b') }}"),
                ("models/d.sql", "select 1"),
            ],
            Dialect::Postgres,
        )
        .and_then(|s| build_order(&s.manifest))
        .unwrap_err();
        assert_eq!(err.0, "dependency cycle: a -> c -> b -> a");
    }
}
