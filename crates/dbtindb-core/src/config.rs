//! Node config: resolution across `dbt_project.yml`, property files and
//! `config()` calls, and typed access to the keys dbtindb acts on.

use serde_json::{Map, Value};

use crate::Error;

/// Config keys that dbt accepts without the `+` prefix in
/// `dbt_project.yml`. Any other unprefixed key is a directory name.
const KNOWN_KEYS: &[&str] = &[
    "enabled",
    "materialized",
    "schema",
    "alias",
    "tags",
    "meta",
    "docs",
    "persist_docs",
    "pre-hook",
    "post-hook",
    "pre_hook",
    "post_hook",
    "unique_key",
    "incremental_strategy",
    "on_schema_change",
    "full_refresh",
    "column_types",
    "delimiter",
    "quote_columns",
    "severity",
    "where",
    "warn_if",
    "error_if",
    "limit",
    "fail_calc",
    "strategy",
    "updated_at",
    "check_cols",
    "target_schema",
    "invalidate_hard_deletes",
    "hard_deletes",
    "group",
    "access",
    "contract",
    "grants",
    "database",
    "store_failures",
    "description",
];

/// A node's resolved config.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config(pub Map<String, Value>);

impl Config {
    /// Layers `other` over `self` with dbt's rules: `tags` and hooks
    /// accumulate, `meta` merges key by key, `grants` privilege by privilege
    /// (a `+select` key adds grantees instead of replacing them), and
    /// everything else is replaced.
    pub fn merge(&mut self, other: &Map<String, Value>) {
        for (key, value) in other {
            let key = normalize(key);
            match key.as_str() {
                "tags" | "pre_hook" | "post_hook" => {
                    let mut items = as_list(self.0.get(&key));
                    for item in as_list(Some(value)) {
                        if key != "tags" || !items.contains(&item) {
                            items.push(item);
                        }
                    }
                    self.0.insert(key, Value::Array(items));
                }
                "meta" => {
                    let mut meta = self
                        .0
                        .get("meta")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    if let Some(m) = value.as_object() {
                        meta.extend(m.clone());
                    }
                    self.0.insert(key, Value::Object(meta));
                }
                "grants" => {
                    let mut grants = self
                        .0
                        .get("grants")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    for (privilege, grantees) in value.as_object().into_iter().flatten() {
                        match privilege.strip_prefix('+') {
                            Some(privilege) => {
                                let mut items = as_list(grants.get(privilege));
                                for g in as_list(Some(grantees)) {
                                    if !items.contains(&g) {
                                        items.push(g);
                                    }
                                }
                                grants.insert(privilege.to_owned(), Value::Array(items));
                            }
                            None => {
                                grants.insert(privilege.clone(), grantees.clone());
                            }
                        }
                    }
                    self.0.insert(key, Value::Object(grants));
                }
                _ => {
                    self.0.insert(key, value.clone());
                }
            }
        }
    }

    /// The config `dbt_project.yml` gives a node: every level of `tree`
    /// along its fully qualified name, outermost first.
    pub fn from_project(tree: &Value, fqn: &[String]) -> Config {
        let mut config = Config::default();
        let mut level = tree;
        let mut path = fqn.iter();
        while let Some(map) = level.as_object() {
            let mut here = Map::new();
            for (k, v) in map {
                if let Some(key) = k.strip_prefix('+') {
                    here.insert(key.to_owned(), v.clone());
                } else if KNOWN_KEYS.contains(&k.as_str()) && !v.is_object() {
                    here.insert(k.clone(), v.clone());
                }
            }
            config.merge(&here);
            match path.next().and_then(|segment| map.get(segment)) {
                Some(next) => level = next,
                None => break,
            }
        }
        config
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key).filter(|v| !v.is_null())
    }

    pub fn str(&self, key: &str) -> Result<Option<&str>, Error> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(other) => Err(Error::new(format!(
                "config {key} must be a string, not {other}"
            ))),
        }
    }

    pub fn bool(&self, key: &str) -> Result<Option<bool>, Error> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(other) => Err(Error::new(format!(
                "config {key} must be true or false, not {other}"
            ))),
        }
    }

    /// A key that may be one string or a list of strings.
    pub fn strings(&self, key: &str) -> Result<Vec<String>, Error> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::String(s)) => Ok(vec![s.clone()]),
            Some(Value::Array(items)) => items
                .iter()
                .map(|i| {
                    i.as_str().map(str::to_owned).ok_or_else(|| {
                        Error::new(format!(
                            "config {key} must be a string or a list of strings"
                        ))
                    })
                })
                .collect(),
            Some(other) => Err(Error::new(format!(
                "config {key} must be a string or a list of strings, not {other}"
            ))),
        }
    }

    pub fn enabled(&self) -> Result<bool, Error> {
        Ok(self.bool("enabled")?.unwrap_or(true))
    }

    pub fn tags(&self) -> Result<Vec<String>, Error> {
        self.strings("tags")
    }
}

/// `dbt_project.yml` spells hooks with dashes, `config()` with
/// underscores; both mean the same key.
fn normalize(key: &str) -> String {
    match key {
        "pre-hook" => "pre_hook".into(),
        "post-hook" => "post_hook".into(),
        k => k.to_owned(),
    }
}

fn as_list(v: Option<&Value>) -> Vec<Value> {
    match v {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(other) => vec![other.clone()],
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fqn(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn project_config_applies_outer_to_inner() {
        let tree = json!({
            "+materialized": "view",
            "+tags": ["all"],
            "shop": {
                "marts": {"+materialized": "table", "+tags": "mart", "finance": {"schema": "fin"}},
                "staging": {"materialized": "view"}
            }
        });
        let c = Config::from_project(&tree, &fqn(&["shop", "marts", "finance", "revenue"]));
        assert_eq!(c.0["materialized"], "table");
        assert_eq!(c.0["schema"], "fin");
        assert_eq!(c.0["tags"], json!(["all", "mart"]));
        let c = Config::from_project(&tree, &fqn(&["shop", "staging", "stg"]));
        assert_eq!(c.0["materialized"], "view");
    }

    #[test]
    fn merge_accumulates_tags_and_hooks() {
        let mut c = Config::default();
        c.merge(
            json!({"tags": "a", "pre-hook": "select 1", "meta": {"x": 1}})
                .as_object()
                .unwrap(),
        );
        c.merge(
            json!({"tags": ["a", "b"], "pre_hook": ["select 2"], "meta": {"y": 2}})
                .as_object()
                .unwrap(),
        );
        assert_eq!(c.tags().unwrap(), ["a", "b"]);
        assert_eq!(c.strings("pre_hook").unwrap(), ["select 1", "select 2"]);
        assert_eq!(c.0["meta"], json!({"x": 1, "y": 2}));

        let mut c = Config::default();
        c.merge(
            json!({"grants": {"select": ["a"], "insert": "w"}})
                .as_object()
                .unwrap(),
        );
        c.merge(
            json!({"grants": {"+select": ["b", "a"], "insert": []}})
                .as_object()
                .unwrap(),
        );
        assert_eq!(c.0["grants"], json!({"select": ["a", "b"], "insert": []}));
    }
}
