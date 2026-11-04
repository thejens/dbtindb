//! Dependency order of compiled models.

use std::collections::{BTreeMap, BTreeSet};

use crate::{CompiledModel, Error};

/// Returns indexes into `models` in an order where every model comes after
/// the models it `ref()`s. Ties are broken by name, so the order is stable.
pub fn build_order(models: &[CompiledModel]) -> Result<Vec<usize>, Error> {
    let index: BTreeMap<&str, usize> = models
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name.as_str(), i))
        .collect();

    let mut remaining: BTreeMap<&str, usize> = models
        .iter()
        .map(|m| (m.name.as_str(), m.depends_on.len()))
        .collect();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for m in models {
        for dep in &m.depends_on {
            if !index.contains_key(dep.as_str()) {
                return Err(Error::new(format!(
                    "model {:?} depends on {dep:?}, which was not compiled",
                    m.name
                )));
            }
            dependents.entry(dep.as_str()).or_default().push(&m.name);
        }
    }

    let mut ready: BTreeSet<&str> = remaining
        .iter()
        .filter(|&(_, &n)| n == 0)
        .map(|(&name, _)| name)
        .collect();
    let mut order = Vec::with_capacity(models.len());
    while let Some(name) = ready.pop_first() {
        remaining.remove(name);
        order.push(index[name]);
        for &d in dependents.get(name).into_iter().flatten() {
            let n = remaining.get_mut(d).expect("dependent is still pending");
            *n -= 1;
            if *n == 0 {
                ready.insert(d);
            }
        }
    }

    if !remaining.is_empty() {
        return Err(Error::new(format!(
            "dependency cycle: {}",
            find_cycle(models, &index, &remaining).join(" -> ")
        )));
    }
    Ok(order)
}

/// Finds one cycle among the models left over by the topological sort, to
/// name it in the error.
fn find_cycle<'a>(
    models: &'a [CompiledModel],
    index: &BTreeMap<&str, usize>,
    remaining: &BTreeMap<&'a str, usize>,
) -> Vec<&'a str> {
    // Every leftover model has a leftover dependency, so walking those
    // edges from any leftover model must revisit one.
    let mut path: Vec<&str> = Vec::new();
    let mut current = *remaining.keys().next().expect("a cycle has members");
    loop {
        if let Some(pos) = path.iter().position(|&p| p == current) {
            let mut cycle = path[pos..].to_vec();
            cycle.push(current);
            return cycle;
        }
        path.push(current);
        current = models[index[current]]
            .depends_on
            .iter()
            .map(String::as_str)
            .find(|d| remaining.contains_key(d))
            .expect("a leftover model has a leftover dependency");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Materialization;

    fn model(name: &str, deps: &[&str]) -> CompiledModel {
        CompiledModel {
            name: name.into(),
            path: format!("models/{name}.sql"),
            relation: name.into(),
            sql: String::new(),
            materialized: Materialization::View,
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            sources: BTreeSet::new(),
        }
    }

    fn names(models: &[CompiledModel], order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| models[i].name.clone()).collect()
    }

    #[test]
    fn orders_dependencies_first_and_stably() {
        let models = vec![
            model("orders", &["stg_orders", "stg_customers"]),
            model("stg_customers", &[]),
            model("stg_orders", &[]),
            model("report", &["orders"]),
        ];
        let order = build_order(&models).unwrap();
        assert_eq!(
            names(&models, &order),
            ["stg_customers", "stg_orders", "orders", "report"]
        );
    }

    #[test]
    fn names_the_cycle() {
        let models = vec![
            model("a", &["c"]),
            model("b", &["a"]),
            model("c", &["b"]),
            model("d", &[]),
        ];
        let err = build_order(&models).unwrap_err();
        assert_eq!(err.0, "dependency cycle: a -> c -> b -> a");
    }
}
