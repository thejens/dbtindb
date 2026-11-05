//! Compiling dbt unit tests: the model under test, rendered with every
//! `ref()` and `source()` it reads (and `this`, when it runs
//! incrementally) replaced by a CTE of fixture rows.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::compile::{inject_ctes, trim_statement};
use crate::context::{Fixtures, Relation, overriding};
use crate::node::{Node, NodeKind};
use crate::project::unit_tests::{Rows, UnitTestDef};
use crate::session::Session;
use crate::{Column, Config, Dialect, Error, SeedType, seed};

/// What a `given:` entry stands in for.
#[derive(Debug, PartialEq)]
enum Input {
    /// `ref('name')` or `ref('package', 'name')`.
    Ref(Option<String>, String),
    Source(String, String),
    This,
}

impl Session {
    /// The SQL of the model a unit test tests, reading fixture rows instead
    /// of its inputs.
    pub fn compile_unit_test(&self, node: &Node) -> Result<String, Error> {
        let NodeKind::UnitTest(def) = &node.kind else {
            unreachable!("only unit tests are compiled as unit tests");
        };
        let model = self.unit_test_model(node, def)?;
        let d = self.shared.dialect;
        let err = |what: String| Error::new(format!("unit test {}: {what}", node.name));
        let mut fixtures = Fixtures {
            vars: def.vars.clone(),
            env_vars: def.env_vars.clone(),
            ..Fixtures::default()
        };
        let overrides = self.macro_overrides(&def.macros).map_err(err)?;
        let mut this = None;
        let mut defs = Vec::new();
        for given in &def.given {
            let input = parse_input(&given.input).map_err(err)?;
            let mut ref_target = None;
            let (cte_name, real) = match &input {
                Input::Ref(package, name) => {
                    let target = self
                        .shared
                        .resolve_ref(package.as_deref(), name, &model.package)
                        .map_err(|e| err(format!("given {}: {e}", given.input)))?;
                    let real = (!target.ephemeral).then_some(target.relation.clone());
                    ref_target = Some(target.unique_id);
                    // Named inputs of two packages can share a model name.
                    let cte_name = match package {
                        Some(package) => format!("{package}_{name}"),
                        None => name.clone(),
                    };
                    (cte_name, real)
                }
                Input::Source(source, table) => {
                    let (schema, identifier) =
                        match self.shared.sources.get(&(source.clone(), table.clone())) {
                            Some(t) => (t.schema.clone(), t.identifier.clone()),
                            None => (source.clone(), table.clone()),
                        };
                    (
                        format!("source_{source}_{table}"),
                        Some(self.shared.relation(&schema, &identifier)),
                    )
                }
                Input::This => ("this".to_owned(), model.relation.clone()),
            };
            let columns = match (&given.rows, &real) {
                (Rows::Sql(_), _) => Vec::new(),
                (_, Some(rel)) => self.db()?.columns(&rel.schema, &rel.identifier)?,
                (_, None) => Vec::new(),
            };
            if columns.is_empty() && !matches!(given.rows, Rows::Sql(_)) {
                return Err(err(format!(
                    "{} has no relation in the database to read its columns from; \
                     build it first, or give its rows with format: sql",
                    given.input
                )));
            }
            let sql = fixture_sql(d, &given.rows, &columns, &given.input).map_err(err)?;
            let cte = Relation::cte(d, &format!("fixture_{cte_name}"));
            defs.push((cte.rendered.clone(), sql));
            match input {
                Input::Ref(..) => {
                    let uid = ref_target.expect("a ref input resolves to a node");
                    fixtures.refs.insert(uid, cte);
                }
                Input::Source(s, t) => {
                    fixtures.sources.insert((s, t), cte);
                }
                Input::This => this = Some(cte),
            }
        }
        if def.is_incremental && this.is_none() {
            return Err(err(
                "with is_incremental overridden to true, give the rows of `this` too".into(),
            ));
        }

        self.begin_execute(model, def.is_incremental);
        {
            let mut cur = self.shared.current.lock().unwrap();
            cur.fixtures = Some(fixtures.clone());
            if this.is_some() {
                cur.this = this;
            }
        }
        let rendered = self.render_file_with(&model.path, overrides);
        let cur = self.shared.finish();
        let rendered = rendered.map_err(|e| err(format!("compiling {}: {e}", model.path)))?;
        // An input given but never read is a mistake in the test, as in dbt.
        if let Some(uid) = fixtures.refs.keys().find(|u| !cur.refs.contains(*u)) {
            return Err(err(format!("{} does not ref {uid}", model.name)));
        }
        if let Some((s, t)) = fixtures.sources.keys().find(|k| !cur.sources.contains(*k)) {
            return Err(err(format!("{} does not read source {s}.{t}", model.name)));
        }
        Ok(inject_ctes(&trim_statement(&rendered), &defs))
    }

    /// What `overrides: {macros: ...}` puts in the model's context: each
    /// named macro replaced by one that returns the given value. As in
    /// dbt, this changes what the model's own SQL calls, not what other
    /// macros call.
    fn macro_overrides(&self, macros: &Map<String, Value>) -> Result<minijinja::Value, String> {
        let mut bare = BTreeMap::new();
        let mut namespaced: BTreeMap<String, BTreeMap<String, minijinja::Value>> = BTreeMap::new();
        for (name, value) in macros {
            if !self.has_macro(name) {
                return Err(format!(
                    "overrides.macros names {name:?}, which is not a macro"
                ));
            }
            let fixed = overriding(value);
            match name.split_once('.') {
                Some((package, macro_name)) => {
                    // dbt's own macros are also called bare.
                    if package == crate::context::DBT_PACKAGE {
                        bare.insert(macro_name.to_owned(), fixed.clone());
                    }
                    namespaced
                        .entry(package.to_owned())
                        .or_default()
                        .insert(macro_name.to_owned(), fixed);
                }
                None => {
                    bare.insert(name.clone(), fixed);
                }
            }
        }
        for (package, names) in namespaced {
            let original = self.env.get_global(&package);
            bare.insert(
                package,
                crate::context::overridden_namespace(original, names),
            );
        }
        Ok(minijinja::Value::from_iter(bare))
    }

    /// The model a unit test tests.
    pub fn unit_test_model<'a>(
        &'a self,
        node: &Node,
        def: &UnitTestDef,
    ) -> Result<&'a Node, Error> {
        let uid = format!("model.{}.{}", node.package, def.model);
        self.manifest.nodes.get(&uid).ok_or_else(|| {
            Error::new(format!(
                "{}: unit test {}: there is no enabled model named {:?}",
                node.path, node.name, def.model
            ))
        })
    }

    fn db(&self) -> Result<&dyn crate::Database, Error> {
        self.shared
            .db
            .as_deref()
            .ok_or_else(|| Error::new("unit tests need a database to read column types from"))
    }
}

fn parse_input(input: &str) -> Result<Input, String> {
    if input == "this" {
        return Ok(Input::This);
    }
    let bad = || format!("input {input:?} must be ref('...'), source('...', '...') or this");
    let (func, rest) = input.split_once('(').ok_or_else(bad)?;
    let args: Vec<String> = rest
        .strip_suffix(')')
        .ok_or_else(bad)?
        .split(',')
        .map(|a| {
            let a = a.trim();
            a.strip_prefix('\'')
                .and_then(|a| a.strip_suffix('\''))
                .or_else(|| a.strip_prefix('"').and_then(|a| a.strip_suffix('"')))
                .map(str::to_owned)
                .ok_or_else(bad)
        })
        .collect::<Result<_, _>>()?;
    match (func.trim(), args.as_slice()) {
        ("ref", [name]) => Ok(Input::Ref(None, name.clone())),
        ("ref", [package, name]) => Ok(Input::Ref(Some(package.clone()), name.clone())),
        ("source", [source, table]) => Ok(Input::Source(source.clone(), table.clone())),
        _ => Err(bad()),
    }
}

/// The names of the columns fixture rows give values for, in order of
/// first appearance; `None` for SQL, whose columns only the database
/// knows.
pub fn row_columns(d: Dialect, rows: &Rows) -> Result<Option<Vec<String>>, Error> {
    Ok(match rows {
        Rows::Sql(_) => None,
        Rows::Dict(rows) => {
            let mut names: Vec<String> = Vec::new();
            for key in rows.iter().flat_map(|r| r.keys()) {
                if !names.contains(key) {
                    names.push(key.clone());
                }
            }
            Some(names)
        }
        Rows::Csv(text) => Some(
            seed::parse("csv rows", text, d, &Config::default())?
                .columns
                .into_iter()
                .map(|(name, _)| name)
                .collect(),
        ),
    })
}

/// A SELECT of fixture rows with exactly `columns`, each value cast to
/// its column's type; columns a row leaves out are NULL.
pub fn fixture_sql(
    d: Dialect,
    rows: &Rows,
    columns: &[Column],
    what: &str,
) -> Result<String, String> {
    let rows: Vec<Map<String, Value>> = match rows {
        Rows::Sql(sql) => return Ok(trim_statement(sql)),
        Rows::Dict(rows) => rows.clone(),
        Rows::Csv(text) => {
            let parsed = seed::parse(what, text, d, &Config::default()).map_err(|e| e.0)?;
            parsed
                .rows
                .into_iter()
                .map(|row| {
                    parsed
                        .columns
                        .iter()
                        .zip(row)
                        .map(|((name, ty), v)| {
                            // Without a declared type (SQLite views), the
                            // value is typed as a seed would be.
                            let v = match v {
                                Some(v) => typed_csv_value(d, ty, &v),
                                None => Value::Null,
                            };
                            (name.clone(), v)
                        })
                        .collect()
                })
                .collect()
        }
    };
    for row in &rows {
        if let Some(k) = row.keys().find(|k| !columns.iter().any(|c| &c.name == *k)) {
            return Err(format!("{what} has no column named {k:?}"));
        }
    }
    let select = |row: Option<&Map<String, Value>>| {
        columns
            .iter()
            .map(|c| {
                let v = row.and_then(|r| r.get(&c.name)).unwrap_or(&Value::Null);
                let lit = literal(d, v);
                let value = if c.data_type.is_empty() {
                    lit
                } else {
                    format!("CAST({lit} AS {})", c.data_type)
                };
                format!("{value} AS {}", d.quote(&c.name))
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    if rows.is_empty() {
        return Ok(format!("SELECT {} WHERE 1 = 0", select(None)));
    }
    Ok(rows
        .iter()
        .map(|r| format!("SELECT {}", select(Some(r))))
        .collect::<Vec<_>>()
        .join("\nUNION ALL\n"))
}

/// A CSV value: a number when it reads as one and the seed type inferred
/// for its column is numeric, otherwise text for the cast to convert.
fn typed_csv_value(d: Dialect, seed_type: &str, v: &str) -> Value {
    let numeric = [SeedType::Integer, SeedType::Number, SeedType::Boolean]
        .iter()
        .any(|t| d.seed_type(*t) == seed_type);
    if numeric && let Ok(n) = v.parse::<serde_json::Number>() {
        return Value::Number(n);
    }
    Value::String(v.to_owned())
}

fn literal(d: Dialect, v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => d.literal(s),
        other => d.literal(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_inputs() {
        assert_eq!(parse_input("ref('a')"), Ok(Input::Ref(None, "a".into())));
        assert_eq!(
            parse_input("ref(\"p\", \"a\")"),
            Ok(Input::Ref(Some("p".into()), "a".into()))
        );
        assert_eq!(
            parse_input("source('raw', 'orders')"),
            Ok(Input::Source("raw".into(), "orders".into()))
        );
        assert_eq!(parse_input("this"), Ok(Input::This));
        assert!(parse_input("analytics.a").is_err());
    }

    #[test]
    fn fixture_rows_are_cast_and_complete() {
        let cols = [
            Column {
                name: "id".into(),
                data_type: "integer".into(),
            },
            Column {
                name: "name".into(),
                data_type: "text".into(),
            },
        ];
        let rows = Rows::Dict(vec![
            json!({"id": 1}).as_object().unwrap().clone(),
            json!({"name": "o'k"}).as_object().unwrap().clone(),
        ]);
        assert_eq!(
            fixture_sql(Dialect::Postgres, &rows, &cols, "ref('a')").unwrap(),
            "SELECT CAST(1 AS integer) AS \"id\", CAST(NULL AS text) AS \"name\"\nUNION ALL\n\
             SELECT CAST(NULL AS integer) AS \"id\", CAST('o''k' AS text) AS \"name\""
        );
        assert_eq!(
            fixture_sql(Dialect::Postgres, &Rows::Dict(vec![]), &cols[..1], "x").unwrap(),
            "SELECT CAST(NULL AS integer) AS \"id\" WHERE 1 = 0"
        );
        let bad = Rows::Dict(vec![json!({"nope": 1}).as_object().unwrap().clone()]);
        assert!(
            fixture_sql(Dialect::Postgres, &bad, &cols, "ref('a')")
                .unwrap_err()
                .contains("no column named \"nope\"")
        );
    }
}
