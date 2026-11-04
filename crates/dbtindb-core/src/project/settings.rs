//! `dbt_project.yml`: the project's name, paths, vars, config and hooks.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::{Error, yaml};

/// The path of the project file among the template rows.
pub const PROJECT_FILE: &str = "dbt_project.yml";

/// What `dbt_project.yml` sets. Without one, a project gets dbt's defaults.
#[derive(Clone, Debug)]
pub struct Settings {
    pub name: String,
    pub model_paths: Vec<String>,
    pub seed_paths: Vec<String>,
    pub snapshot_paths: Vec<String>,
    pub test_paths: Vec<String>,
    pub macro_paths: Vec<String>,
    /// Where `dbt deps` installs packages: `dbt_packages` by default.
    pub packages_install_path: String,
    /// `dispatch:` search orders, by macro namespace: which packages
    /// `adapter.dispatch(name, namespace)` searches, in order.
    pub dispatch: BTreeMap<String, Vec<String>>,
    /// `vars:`, both global ones and those scoped to this project's name.
    pub vars: Map<String, Value>,
    /// The `models:`, `seeds:`, `snapshots:` and `data_tests:` config trees.
    pub models: Value,
    pub seeds: Value,
    pub snapshots: Value,
    pub tests: Value,
    pub on_run_start: Vec<String>,
    pub on_run_end: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            name: "dbtindb".into(),
            model_paths: vec!["models".into()],
            seed_paths: vec!["seeds".into()],
            snapshot_paths: vec!["snapshots".into()],
            test_paths: vec!["tests".into()],
            macro_paths: vec!["macros".into()],
            packages_install_path: "dbt_packages".into(),
            dispatch: BTreeMap::new(),
            vars: Map::new(),
            models: Value::Null,
            seeds: Value::Null,
            snapshots: Value::Null,
            tests: Value::Null,
            on_run_start: Vec::new(),
            on_run_end: Vec::new(),
        }
    }
}

impl Settings {
    pub fn parse(text: &str) -> Result<Self, Error> {
        let doc = yaml::parse(PROJECT_FILE, text)?;
        let doc = doc.as_object().expect("yaml::parse returns a mapping");
        let mut s = Settings::default();
        if let Some(name) = doc.get("name") {
            s.name = name
                .as_str()
                .ok_or_else(|| field_error("name", "a string"))?
                .to_owned();
        }
        for (key, slot) in [
            ("model-paths", &mut s.model_paths),
            ("seed-paths", &mut s.seed_paths),
            ("snapshot-paths", &mut s.snapshot_paths),
            ("test-paths", &mut s.test_paths),
            ("macro-paths", &mut s.macro_paths),
        ] {
            if let Some(v) = doc.get(key) {
                *slot = string_list(v).ok_or_else(|| field_error(key, "a list of paths"))?;
                for p in slot.iter_mut() {
                    *p = p.trim_end_matches('/').to_owned();
                }
            }
        }
        if let Some(vars) = doc.get("vars") {
            let vars = vars
                .as_object()
                .ok_or_else(|| field_error("vars", "a mapping"))?;
            for (k, v) in vars {
                // `vars: {my_project: {x: 1}}` scopes vars to a project;
                // this is the only project, so they are global here.
                match v.as_object() {
                    Some(scoped) if *k == s.name => {
                        s.vars.extend(scoped.clone());
                    }
                    _ => {
                        s.vars.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        s.models = doc.get("models").cloned().unwrap_or(Value::Null);
        s.seeds = doc.get("seeds").cloned().unwrap_or(Value::Null);
        s.snapshots = doc.get("snapshots").cloned().unwrap_or(Value::Null);
        s.tests = doc
            .get("data_tests")
            .or_else(|| doc.get("tests"))
            .cloned()
            .unwrap_or(Value::Null);
        for (key, slot) in [
            ("on-run-start", &mut s.on_run_start),
            ("on-run-end", &mut s.on_run_end),
        ] {
            if let Some(v) = doc.get(key) {
                *slot =
                    hooks(v).ok_or_else(|| field_error(key, "a string or a list of strings"))?;
            }
        }
        if let Some(v) = doc.get("packages-install-path") {
            s.packages_install_path = v
                .as_str()
                .ok_or_else(|| field_error("packages-install-path", "a path"))?
                .trim_end_matches('/')
                .to_owned();
        }
        if let Some(v) = doc.get("dispatch") {
            s.dispatch = dispatch(v).ok_or_else(|| {
                field_error(
                    "dispatch",
                    "a list of {macro_namespace: <package>, search_order: [<package>, ...]}",
                )
            })?;
        }
        Ok(s)
    }
}

/// The `dispatch:` list, keyed by macro namespace.
fn dispatch(v: &Value) -> Option<BTreeMap<String, Vec<String>>> {
    let mut orders = BTreeMap::new();
    for entry in v.as_array()? {
        let namespace = entry.get("macro_namespace")?.as_str()?;
        let order = string_list(entry.get("search_order")?)?;
        orders.insert(namespace.to_owned(), order);
    }
    Some(orders)
}

/// A hook setting: one SQL string or a list of them.
fn hooks(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Null => Some(Vec::new()),
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(items) => items
            .iter()
            .map(|i| i.as_str().map(str::to_owned))
            .collect(),
        _ => None,
    }
}

fn string_list(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(items) => items
            .iter()
            .map(|i| i.as_str().map(str::to_owned))
            .collect(),
        _ => None,
    }
}

fn field_error(key: &str, expected: &str) -> Error {
    Error::new(format!("{PROJECT_FILE}: {key} must be {expected}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_paths_vars_and_hooks() {
        let s = Settings::parse(
            "name: shop\nmodel-paths: [transform/]\nvars:\n  a: 1\n  shop:\n    b: 2\non-run-end: \"grant select on all tables in schema x to y\"\n",
        )
        .unwrap();
        assert_eq!(s.name, "shop");
        assert_eq!(s.model_paths, ["transform"]);
        assert_eq!(s.seed_paths, ["seeds"]);
        assert_eq!(Value::Object(s.vars), serde_json::json!({"a": 1, "b": 2}));
        assert_eq!(s.on_run_end.len(), 1);
        assert_eq!(s.packages_install_path, "dbt_packages");
    }

    #[test]
    fn reads_dispatch_and_install_path() {
        let s = Settings::parse(
            "name: shop\npackages-install-path: deps/\ndispatch:\n  - macro_namespace: dbt_utils\n    search_order: [shop, dbt_utils]\n",
        )
        .unwrap();
        assert_eq!(s.packages_install_path, "deps");
        assert_eq!(s.dispatch["dbt_utils"], ["shop", "dbt_utils"]);
        let err = Settings::parse("dispatch: [{macro_namespace: x}]\n").unwrap_err();
        assert!(err.0.contains("dispatch must be"), "{err}");
    }
}
