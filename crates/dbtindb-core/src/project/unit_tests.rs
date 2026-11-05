//! dbt 1.8 unit tests, from `unit_tests:` in property files: a model run
//! on fixed input rows, its output compared with the expected rows.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::properties::{list, optional_string, string};
use crate::Error;

/// Fixture files, `<test-path>/fixtures/**/<name>.csv` or `.sql`, by name
/// and extension: their text.
pub type FixtureFiles = BTreeMap<(String, String), String>;

/// Fixture rows, in one of dbt's three formats.
#[derive(Clone, Debug, PartialEq)]
pub enum Rows {
    /// `format: dict` (the default): a list of mappings, column to value.
    /// Columns a row leaves out are NULL.
    Dict(Vec<Map<String, Value>>),
    /// `format: csv`: a header line, then rows. Empty fields are NULL.
    Csv(String),
    /// `format: sql`: a SELECT that returns the rows.
    Sql(String),
}

/// One `given:` entry: the rows that stand in for an input.
#[derive(Clone, Debug, PartialEq)]
pub struct Given {
    /// `ref('...')`, `source('...', '...')` or `this`, as written.
    pub input: String,
    pub rows: Rows,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnitTestDef {
    /// The property file that declared it.
    pub file: String,
    pub name: String,
    /// The model under test.
    pub model: String,
    pub description: String,
    pub given: Vec<Given>,
    pub expect: Rows,
    /// `overrides: {macros: {is_incremental: true}}`.
    pub is_incremental: bool,
    /// The other `overrides: {macros: ...}`: what each named macro (bare,
    /// or `package.macro`) returns instead of running.
    pub macros: Map<String, Value>,
    /// `overrides: {vars: ...}`, layered over the project's vars.
    pub vars: Map<String, Value>,
    /// `overrides: {env_vars: ...}`, what `env_var()` returns.
    pub env_vars: Map<String, Value>,
    pub config: Map<String, Value>,
}

/// Reads one entry of `unit_tests:`, with the fixture files its rows can
/// name.
pub fn parse(path: &str, entry: &Value, fixtures: &FixtureFiles) -> Result<UnitTestDef, Error> {
    let name = string(entry, "name", &format!("{path}: unit_tests"))?;
    let where_ = format!("{path}: unit test {name:?}");
    let err = |what: &str| Error::new(format!("{where_}: {what}"));
    if entry.get("versions").is_some() {
        return Err(err("unit tests of model versions are not supported"));
    }
    let model = string(entry, "model", &where_)?;
    let given = list(entry.get("given"), &where_)?
        .into_iter()
        .map(|g| {
            Ok(Given {
                input: string(g, "input", &where_)?.trim().to_owned(),
                rows: rows(g, &where_, fixtures)?,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let expect = entry
        .get("expect")
        .ok_or_else(|| err("expect is required"))?;
    let mut def = UnitTestDef {
        file: path.to_owned(),
        description: optional_string(entry, "description")?.unwrap_or_default(),
        given,
        expect: rows(expect, &where_, fixtures)?,
        is_incremental: false,
        macros: Map::new(),
        vars: Map::new(),
        env_vars: Map::new(),
        config: mapping(entry.get("config"), "config", &where_)?,
        name,
        model,
    };
    let overrides = mapping(entry.get("overrides"), "overrides", &where_)?;
    for (key, value) in &overrides {
        match key.as_str() {
            "vars" => def.vars = mapping(Some(value), "overrides.vars", &where_)?,
            "env_vars" => def.env_vars = mapping(Some(value), "overrides.env_vars", &where_)?,
            "macros" => {
                for (m, v) in mapping(Some(value), "overrides.macros", &where_)? {
                    match (m.as_str(), &v) {
                        ("is_incremental", Value::Bool(b)) => def.is_incremental = *b,
                        ("is_incremental", _) => {
                            return Err(err(
                                "overrides.macros.is_incremental must be true or false",
                            ));
                        }
                        _ => {
                            let valid = m.split('.').count() <= 2
                                && m.split('.').all(|part| {
                                    !part.is_empty()
                                        && part
                                            .chars()
                                            .all(|c| c.is_ascii_alphanumeric() || c == '_')
                                });
                            if !valid {
                                return Err(err(&format!(
                                    "overrides.macros: {m:?} is not a macro name \
                                     (`name` or `package.name`)"
                                )));
                            }
                            def.macros.insert(m, v);
                        }
                    }
                }
            }
            other => return Err(err(&format!("unknown override {other:?}"))),
        }
    }
    Ok(def)
}

fn rows(entry: &Value, where_: &str, fixtures: &FixtureFiles) -> Result<Rows, Error> {
    let err = |what: String| Error::new(format!("{where_}: {what}"));
    let format = optional_string(entry, "format")?.unwrap_or_else(|| "dict".into());
    // `fixture: name` reads the rows from `fixtures/name.csv` (or `.sql`)
    // under a test path, in place of inline `rows`.
    let file;
    let rows = match optional_string(entry, "fixture")? {
        Some(_) if entry.get("rows").is_some() => {
            return Err(err("give either rows or a fixture, not both".into()));
        }
        Some(name) => {
            if format == "dict" {
                return Err(err(format!(
                    "fixture {name:?}: fixture files are csv or sql; set format: csv or format: sql"
                )));
            }
            let text = fixtures
                .get(&(name.clone(), format.clone()))
                .ok_or_else(|| {
                    err(format!(
                        "there is no fixture file {name}.{format} in a fixtures directory of the test paths"
                    ))
                })?;
            file = Value::String(text.clone());
            Some(&file)
        }
        None => entry.get("rows"),
    };
    match format.as_str() {
        "dict" => list(rows, where_)?
            .into_iter()
            .map(|row| {
                row.as_object()
                    .cloned()
                    .ok_or_else(|| err(format!("a dict row must be a mapping, not {row}")))
            })
            .collect::<Result<_, _>>()
            .map(Rows::Dict),
        "csv" | "sql" => {
            let text = match rows {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) => s.clone(),
                Some(other) => {
                    return Err(err(format!("{format} rows must be a string, not {other}")));
                }
            };
            Ok(if format == "csv" {
                Rows::Csv(text)
            } else {
                Rows::Sql(text)
            })
        }
        other => Err(err(format!(
            "format {other:?} is not supported (dict, csv, sql)"
        ))),
    }
}

fn mapping(v: Option<&Value>, key: &str, where_: &str) -> Result<Map<String, Value>, Error> {
    match v {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(_) => Err(Error::new(format!("{where_}: {key} must be a mapping"))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_inputs_formats_and_overrides() {
        let def = parse(
            "models/u.yml",
            &json!({
                "name": "t", "model": "m",
                "given": [
                    {"input": "ref('a')", "rows": [{"id": 1}]},
                    {"input": "source('raw', 'b')", "format": "csv", "rows": "id\n1\n"},
                    {"input": "this", "format": "sql", "rows": "select 1 as id"},
                ],
                "expect": {"rows": [{"id": 1}]},
                "overrides": {"macros": {"is_incremental": true}, "vars": {"x": 1}},
            }),
            &FixtureFiles::new(),
        )
        .unwrap();
        assert_eq!(def.given[1].rows, Rows::Csv("id\n1\n".into()));
        assert_eq!(def.given[2].input, "this");
        assert!(def.is_incremental);
        assert_eq!(def.vars["x"], json!(1));

        let def = parse(
            "u.yml",
            &json!({"name": "t", "model": "m", "expect": {"rows": []},
                    "overrides": {"macros": {"dbt_utils.now": "'2024-01-01'"}}}),
            &FixtureFiles::new(),
        )
        .unwrap();
        assert_eq!(def.macros["dbt_utils.now"], json!("'2024-01-01'"));
        let err = parse(
            "u.yml",
            &json!({"name": "t", "model": "m", "expect": {"rows": []},
                    "overrides": {"macros": {"a.b.c": "x"}}}),
            &FixtureFiles::new(),
        )
        .unwrap_err();
        assert!(err.0.contains("is not a macro name"), "{err}");
    }

    #[test]
    fn reads_fixture_files() {
        let fixtures = FixtureFiles::from([(
            ("orders".to_owned(), "csv".to_owned()),
            "id\n1\n".to_owned(),
        )]);
        let def = parse(
            "models/u.yml",
            &json!({
                "name": "t", "model": "m",
                "given": [{"input": "ref('a')", "format": "csv", "fixture": "orders"}],
                "expect": {"format": "csv", "fixture": "orders"},
            }),
            &fixtures,
        )
        .unwrap();
        assert_eq!(def.given[0].rows, Rows::Csv("id\n1\n".into()));
        assert_eq!(def.expect, Rows::Csv("id\n1\n".into()));
        for (entry, msg) in [
            (json!({"fixture": "orders"}), "set format: csv"),
            (
                json!({"format": "sql", "fixture": "orders"}),
                "no fixture file orders.sql",
            ),
            (
                json!({"format": "csv", "fixture": "orders", "rows": "id"}),
                "not both",
            ),
        ] {
            let err = parse(
                "u.yml",
                &json!({"name": "t", "model": "m", "expect": entry}),
                &fixtures,
            )
            .unwrap_err();
            assert!(err.0.contains(msg), "{err}");
        }
    }
}
