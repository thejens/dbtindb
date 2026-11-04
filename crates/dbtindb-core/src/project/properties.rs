//! Property files (`schema.yml` and friends): descriptions, config, data
//! tests and sources.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::{Error, yaml};

/// Everything the project's property files declare, merged.
#[derive(Clone, Debug, Default)]
pub struct Properties {
    pub models: BTreeMap<String, ResourceProps>,
    pub seeds: BTreeMap<String, ResourceProps>,
    pub snapshots: BTreeMap<String, ResourceProps>,
    pub sources: BTreeMap<(String, String), SourceTable>,
}

/// Properties of one model, seed or snapshot.
#[derive(Clone, Debug, Default)]
pub struct ResourceProps {
    /// The property file that declared it.
    pub file: String,
    pub description: String,
    pub config: Map<String, Value>,
    pub columns: Vec<ColumnProps>,
    /// Tests on the whole resource (not on one column).
    pub tests: Vec<TestDef>,
}

#[derive(Clone, Debug, Default)]
pub struct ColumnProps {
    pub name: String,
    pub description: String,
    pub tests: Vec<TestDef>,
}

/// One generic test application, e.g. `- accepted_values: {values: [a]}`.
#[derive(Clone, Debug, PartialEq)]
pub struct TestDef {
    /// The generic test's name: `not_null`, `relationships`, or a custom one.
    pub test: String,
    /// Its arguments, besides `model` and `column_name`.
    pub args: Map<String, Value>,
    pub config: Map<String, Value>,
    /// An explicit name for the test node, from `name:`.
    pub name: Option<String>,
}

/// A table declared under `sources:`.
#[derive(Clone, Debug, Default)]
pub struct SourceTable {
    pub file: String,
    pub source_name: String,
    pub name: String,
    /// The schema it lives in (`schema:` of the source, else its name).
    pub schema: String,
    /// The table's name in the database (`identifier:`, else its name).
    pub identifier: String,
    pub description: String,
    pub columns: Vec<ColumnProps>,
    pub tests: Vec<TestDef>,
}

impl Properties {
    /// Adds the declarations of one property file.
    pub fn add_file(&mut self, path: &str, text: &str) -> Result<(), Error> {
        let doc = yaml::parse(path, text)?;
        let doc = doc.as_object().expect("yaml::parse returns a mapping");
        let ctx = |what: &str| format!("{path}: {what}");
        for (key, target) in [
            ("models", &mut self.models),
            ("seeds", &mut self.seeds),
            ("snapshots", &mut self.snapshots),
        ] {
            for entry in list(doc.get(key), &ctx(key))? {
                let props = resource(path, entry)?;
                let name = string(entry, "name", &ctx(key))?;
                if let Some(prev) = target.insert(name.clone(), props) {
                    return Err(Error::new(format!(
                        "{key} entry {name:?} is declared in both {} and {path}",
                        prev.file
                    )));
                }
            }
        }
        for source in list(doc.get("sources"), &ctx("sources"))? {
            let source_name = string(source, "name", &ctx("sources"))?;
            let schema = optional_string(source, "schema")?.unwrap_or_else(|| source_name.clone());
            let where_ = ctx(&format!("source {source_name:?}"));
            for table in list(source.get("tables"), &where_)? {
                let name = string(table, "name", &where_)?;
                let props = resource(path, table)?;
                let entry = SourceTable {
                    file: path.to_owned(),
                    source_name: source_name.clone(),
                    identifier: optional_string(table, "identifier")?
                        .unwrap_or_else(|| name.clone()),
                    name: name.clone(),
                    schema: schema.clone(),
                    description: props.description,
                    columns: props.columns,
                    tests: props.tests,
                };
                if self
                    .sources
                    .insert((source_name.clone(), name.clone()), entry)
                    .is_some()
                {
                    return Err(Error::new(format!(
                        "source {source_name:?} declares table {name:?} twice"
                    )));
                }
            }
        }
        Ok(())
    }
}

fn resource(path: &str, entry: &Value) -> Result<ResourceProps, Error> {
    let name = entry.get("name").and_then(Value::as_str).unwrap_or("?");
    let where_ = format!("{path}: {name}");
    let mut columns = Vec::new();
    for col in list(entry.get("columns"), &where_)? {
        let col_name = string(col, "name", &where_)?;
        columns.push(ColumnProps {
            description: optional_string(col, "description")?.unwrap_or_default(),
            tests: tests(col, &format!("{where_}.{col_name}"))?,
            name: col_name,
        });
    }
    Ok(ResourceProps {
        file: path.to_owned(),
        description: optional_string(entry, "description")?.unwrap_or_default(),
        config: match entry.get("config") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => return Err(Error::new(format!("{where_}: config must be a mapping"))),
        },
        columns,
        tests: tests(entry, &where_)?,
    })
}

/// Reads `data_tests:` (or the older `tests:`) of a resource or column.
fn tests(entry: &Value, where_: &str) -> Result<Vec<TestDef>, Error> {
    let items = entry.get("data_tests").or_else(|| entry.get("tests"));
    list(items, where_)?
        .into_iter()
        .map(|t| test_def(t, where_))
        .collect()
}

fn test_def(item: &Value, where_: &str) -> Result<TestDef, Error> {
    let (test, body) = match item {
        Value::String(name) => (name.clone(), Map::new()),
        Value::Object(m) if m.len() == 1 => {
            let (name, body) = m.iter().next().expect("one entry");
            let body = match body {
                Value::Null => Map::new(),
                Value::Object(b) => b.clone(),
                _ => {
                    return Err(Error::new(format!(
                        "{where_}: the arguments of test {name:?} must be a mapping"
                    )));
                }
            };
            (name.clone(), body)
        }
        _ => {
            return Err(Error::new(format!(
                "{where_}: a test must be a name or a mapping with one key, the test's name"
            )));
        }
    };
    // dbt's own generic tests may be written with their namespace.
    let test = test.strip_prefix("dbt.").unwrap_or(&test).to_owned();
    if test.contains('.') {
        return Err(Error::new(format!(
            "{where_}: test {test:?} comes from a package; packages are not supported, \
             so define it as a {{% test %}} block in macros/"
        )));
    }
    let mut args = Map::new();
    let mut config = Map::new();
    let mut name = None;
    for (k, v) in body {
        match k.as_str() {
            "config" => config = v.as_object().cloned().unwrap_or_default(),
            "name" => name = v.as_str().map(str::to_owned),
            // dbt 1.10+ nests arguments under `arguments:`.
            "arguments" => args.extend(v.as_object().cloned().unwrap_or_default()),
            // Config keys written next to the arguments, as older dbt allows.
            "severity" | "where" | "warn_if" | "error_if" | "enabled" | "tags" | "limit" => {
                config.insert(k, v);
            }
            _ => {
                args.insert(k, v);
            }
        }
    }
    Ok(TestDef {
        test,
        args,
        config,
        name,
    })
}

fn list<'a>(v: Option<&'a Value>, where_: &str) -> Result<Vec<&'a Value>, Error> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.iter().collect()),
        Some(_) => Err(Error::new(format!("{where_} must be a list"))),
    }
}

fn string(v: &Value, key: &str, where_: &str) -> Result<String, Error> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::new(format!("{where_}: every entry needs a {key}")))
}

fn optional_string(v: &Value, key: &str) -> Result<Option<String>, Error> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(Error::new(format!("{key} must be a string"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_models_columns_tests_and_sources() {
        let mut p = Properties::default();
        p.add_file(
            "models/schema.yml",
            r#"
version: 2
models:
  - name: customers
    description: One row per customer
    config: {materialized: table}
    columns:
      - name: id
        data_tests: [unique, not_null]
      - name: status
        tests:
          - accepted_values:
              values: [active, churned]
              config: {severity: warn}
          - relationships:
              arguments: {to: "ref('statuses')", field: code}
              name: status_is_known
sources:
  - name: raw
    schema: raw_data
    tables:
      - name: orders
        identifier: orders_v2
"#,
        )
        .unwrap();
        let c = &p.models["customers"];
        assert_eq!(c.config["materialized"], "table");
        assert_eq!(c.columns[0].tests[0].test, "unique");
        let av = &c.columns[1].tests[0];
        assert_eq!(av.test, "accepted_values");
        assert_eq!(av.args["values"], serde_json::json!(["active", "churned"]));
        assert_eq!(av.config["severity"], "warn");
        let rel = &c.columns[1].tests[1];
        assert_eq!(rel.args["field"], "code");
        assert_eq!(rel.name.as_deref(), Some("status_is_known"));
        let src = &p.sources[&("raw".to_owned(), "orders".to_owned())];
        assert_eq!(
            (src.schema.as_str(), src.identifier.as_str()),
            ("raw_data", "orders_v2")
        );
    }

    #[test]
    fn rejects_package_tests() {
        let mut p = Properties::default();
        let err = p
            .add_file(
                "models/s.yml",
                "models:\n  - name: m\n    tests: [dbt_utils.at_least_one]\n",
            )
            .unwrap_err();
        assert!(err.0.contains("packages are not supported"), "{err}");
    }
}
