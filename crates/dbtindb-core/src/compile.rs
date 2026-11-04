//! Rendering models with dbt's Jinja dialect and DAG functions.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use minijinja::value::{Kwargs, Rest};
use minijinja::{Environment, ErrorKind, Value};

use crate::{Error, Project, Target};

/// Per-render instruction budget, so a runaway macro cannot pin a database
/// backend: rendering never returns control to the database until it ends.
const FUEL: u64 = 50_000_000;

/// How a model is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Materialization {
    View,
    Table,
}

impl Materialization {
    pub fn as_str(self) -> &'static str {
        match self {
            Materialization::View => "view",
            Materialization::Table => "table",
        }
    }
}

/// A model rendered to plain SQL, with everything its rendering revealed.
#[derive(Clone, Debug)]
pub struct CompiledModel {
    pub name: String,
    pub path: String,
    /// The quoted relation the model is built as, which is also what
    /// `ref()` to it renders to.
    pub relation: String,
    /// The model's SELECT, without a trailing semicolon.
    pub sql: String,
    pub materialized: Materialization,
    /// Models this one `ref()`s.
    pub depends_on: BTreeSet<String>,
    /// Relations this one reads through `source()`.
    pub sources: BTreeSet<String>,
}

/// What `ref`, `source` and `config` recorded during one model's render.
#[derive(Default)]
struct Recorder {
    model: String,
    refs: BTreeSet<String>,
    sources: BTreeSet<String>,
    materialized: Option<Materialization>,
    enabled: Option<bool>,
}

/// `config()` keys that only carry metadata, accepted and ignored so dbt
/// projects compile unchanged. Keys that would change where or how a model
/// is built (schema, alias, unique_key, ...) are rejected instead, rather
/// than silently building something different from what dbt would.
const METADATA_CONFIG_KEYS: &[&str] = &[
    "tags",
    "meta",
    "docs",
    "description",
    "persist_docs",
    "group",
    "access",
];

/// Compiles every enabled model of `project` for `target`.
///
/// `vars` is the object `var()` reads from. Models are returned in name
/// order; use [`crate::build_order`] for dependency order.
pub fn compile(
    project: &Project,
    target: &Target,
    vars: &serde_json::Value,
) -> Result<Vec<CompiledModel>, Error> {
    let vars = match vars {
        serde_json::Value::Null => serde_json::Map::new(),
        serde_json::Value::Object(map) => map.clone(),
        other => {
            return Err(Error::new(format!(
                "vars must be a JSON object, got {other}"
            )));
        }
    };
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let mut env = environment(project, target, vars, recorder.clone());

    let preamble = macro_preamble(&env, project)?;

    let mut compiled = Vec::new();
    let mut disabled = BTreeSet::new();
    for model in project.models.values() {
        let relation = target.dialect.relation(&target.schema, &model.name);
        *recorder.lock().unwrap() = Recorder {
            model: model.name.clone(),
            ..Recorder::default()
        };
        env.add_global("this", Value::from(relation.clone()));
        // The preamble goes on the model's first line so error line numbers
        // still match the model's own source.
        let source = format!("{preamble}{}", project.files[&model.path]);
        env.add_template_owned(model.path.clone(), source, None)
            .map_err(|e| model_error(&model.name, e))?;
        let rendered = env
            .get_template(&model.path)
            .and_then(|t| t.render((), &[]))
            .map_err(|e| model_error(&model.name, e))?;

        let rec = std::mem::take(&mut *recorder.lock().unwrap());
        if rec.enabled == Some(false) {
            disabled.insert(model.name.clone());
            continue;
        }
        compiled.push(CompiledModel {
            name: model.name.clone(),
            path: model.path.clone(),
            relation,
            sql: trim_statement(&rendered),
            materialized: rec.materialized.unwrap_or(Materialization::View),
            depends_on: rec.refs,
            sources: rec.sources,
        });
    }

    // dbt refuses to build a model that refs a disabled one.
    for m in &compiled {
        if let Some(d) = m.depends_on.iter().find(|d| disabled.contains(*d)) {
            return Err(Error::new(format!(
                "model {:?} refs {d:?}, which is disabled (config(enabled=false))",
                m.name
            )));
        }
    }
    Ok(compiled)
}

fn environment(
    project: &Project,
    target: &Target,
    vars: serde_json::Map<String, serde_json::Value>,
    recorder: Arc<Mutex<Recorder>>,
) -> Environment<'static> {
    let mut env = Environment::new();
    env.set_fuel(Some(FUEL));

    let files = Arc::new(project.files.clone());
    env.set_loader(move |name| Ok(files.get(name).cloned()));

    let models: Arc<BTreeSet<String>> = Arc::new(project.models.keys().cloned().collect());
    let (dialect, schema) = (target.dialect, target.schema.clone());
    let rec = recorder.clone();
    env.add_function("ref", move |args: Rest<String>| {
        let name = match args.0.as_slice() {
            [name] => name,
            [_, _] => {
                return Err(jinja_error(
                    "ref() with a package name is not supported: dbtindb projects have no packages",
                ));
            }
            _ => return Err(jinja_error("ref() takes one argument: the model name")),
        };
        let mut rec = rec.lock().unwrap();
        if !models.contains(name) {
            return Err(jinja_error(format!(
                "model {:?} refs {name:?}, which is not a model in this project",
                rec.model
            )));
        }
        rec.refs.insert(name.clone());
        Ok(Value::from(dialect.relation(&schema, name)))
    });

    let rec = recorder.clone();
    env.add_function("source", move |source_name: String, table_name: String| {
        // Without dbt's sources.yml, the source name is the schema the
        // table lives in.
        let relation = dialect.relation(&source_name, &table_name);
        rec.lock().unwrap().sources.insert(relation.clone());
        Ok::<_, minijinja::Error>(Value::from(relation))
    });

    let rec = recorder.clone();
    env.add_function("config", move |kwargs: Kwargs| {
        let mut rec = rec.lock().unwrap();
        for key in kwargs.args().map(str::to_owned).collect::<Vec<_>>() {
            match key.as_str() {
                "materialized" => {
                    let m: String = kwargs.get(&key)?;
                    rec.materialized = Some(match m.as_str() {
                        "view" => Materialization::View,
                        "table" => Materialization::Table,
                        other => {
                            return Err(jinja_error(format!(
                                "materialized={other:?} is not supported by dbtindb (use \"view\" or \"table\")"
                            )));
                        }
                    });
                }
                "enabled" => rec.enabled = Some(kwargs.get(&key)?),
                k if METADATA_CONFIG_KEYS.contains(&k) => {
                    let _: Value = kwargs.get(k)?;
                }
                other => {
                    return Err(jinja_error(format!(
                        "config key {other:?} is not supported by dbtindb"
                    )));
                }
            }
        }
        Ok(Value::from(""))
    });

    let vars = Arc::new(vars);
    env.add_function("var", move |name: String, default: Option<Value>| {
        match (vars.get(&name), default) {
            (Some(v), _) => Ok(Value::from_serialize(v)),
            (None, Some(d)) => Ok(d),
            (None, None) => Err(jinja_error(format!(
                "required var {name:?} not found; pass it in the vars argument"
            ))),
        }
    });

    // dbtindb always rebuilds models from scratch, which is exactly how dbt
    // treats an incremental model on a full refresh.
    env.add_function("is_incremental", || Ok::<_, minijinja::Error>(false));

    env.add_global(
        "target",
        Value::from_serialize(serde_json::json!({
            "name": "dbtindb",
            "type": target.dialect.name(),
            "schema": target.schema,
        })),
    );
    env
}

/// Builds the `{% from ... import ... %}` line that makes every macro in
/// `macros/` callable from models without an import, as in dbt.
fn macro_preamble(env: &Environment<'static>, project: &Project) -> Result<String, Error> {
    let mut owner: BTreeMap<String, String> = BTreeMap::new();
    let mut preamble = String::new();
    for path in &project.macro_files {
        let template = env.get_template(path)?;
        let state = template
            .eval_to_state((), &[])
            .map_err(|e| Error::new(format!("loading macros from {path}: {e}")))?;
        let mut names: Vec<&str> = state.exports();
        names.sort_unstable();
        if names.is_empty() {
            continue;
        }
        for name in &names {
            if let Some(other) = owner.insert(name.to_string(), path.clone()) {
                return Err(Error::new(format!(
                    "macro {name:?} is defined in both {other} and {path}"
                )));
            }
        }
        preamble.push_str(&format!(
            "{{%- from {path:?} import {} -%}}",
            names.join(", ")
        ));
    }
    Ok(preamble)
}

fn model_error(model: &str, e: minijinja::Error) -> Error {
    Error::new(format!("compiling model {model:?}: {e}"))
}

fn jinja_error(msg: impl Into<String>) -> minijinja::Error {
    minijinja::Error::new(ErrorKind::InvalidOperation, msg.into())
}

/// Trims whitespace and one trailing semicolon, so the SELECT can be
/// wrapped in CREATE VIEW/TABLE ... AS.
fn trim_statement(sql: &str) -> String {
    let sql = sql.trim();
    sql.strip_suffix(';').unwrap_or(sql).trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dialect;

    fn project(files: &[(&str, &str)]) -> Project {
        Project::new(files.iter().map(|(p, s)| (p.to_string(), s.to_string()))).unwrap()
    }

    fn pg() -> Target {
        Target {
            dialect: Dialect::Postgres,
            schema: "analytics".into(),
        }
    }

    fn compile_ok(files: &[(&str, &str)]) -> BTreeMap<String, CompiledModel> {
        compile(&project(files), &pg(), &serde_json::json!({}))
            .unwrap()
            .into_iter()
            .map(|m| (m.name.clone(), m))
            .collect()
    }

    #[test]
    fn ref_and_source_resolve_and_are_recorded() {
        let m = compile_ok(&[
            (
                "models/stg_orders.sql",
                "select * from {{ source('raw', 'orders') }}",
            ),
            (
                "models/orders.sql",
                "select id from {{ ref('stg_orders') }};\n",
            ),
        ]);
        assert_eq!(m["stg_orders"].sql, r#"select * from "raw"."orders""#);
        assert_eq!(
            m["stg_orders"].sources,
            BTreeSet::from([r#""raw"."orders""#.to_owned()])
        );
        assert_eq!(
            m["orders"].sql,
            r#"select id from "analytics"."stg_orders""#
        );
        assert_eq!(
            m["orders"].depends_on,
            BTreeSet::from(["stg_orders".to_owned()])
        );
        assert_eq!(m["orders"].relation, r#""analytics"."orders""#);
    }

    #[test]
    fn config_sets_materialization_and_defaults_to_view() {
        let m = compile_ok(&[
            (
                "models/a.sql",
                "{{ config(materialized='table', tags=['x']) }}select 1",
            ),
            ("models/b.sql", "select 2"),
        ]);
        assert_eq!(m["a"].materialized, Materialization::Table);
        assert_eq!(m["a"].sql, "select 1");
        assert_eq!(m["b"].materialized, Materialization::View);
    }

    #[test]
    fn macros_are_global_and_dbt_blocks_parse() {
        let m = compile_ok(&[
            (
                "macros/money.sql",
                "{% macro cents_to_dollars(col) %}({{ col }} / 100.0){% endmacro %}\
                 {% docs money %}Money helpers{% enddocs %}",
            ),
            (
                "macros/util.sql",
                "{% macro schema_of() %}{{ target.schema }}{% endmacro %}",
            ),
            (
                "models/p.sql",
                "select {{ cents_to_dollars('amount') }} from {{ this }} -- {{ schema_of() }}",
            ),
        ]);
        assert_eq!(
            m["p"].sql,
            r#"select (amount / 100.0) from "analytics"."p" -- analytics"#
        );
    }

    #[test]
    fn var_reads_vars_with_defaults() {
        let p = project(&[("models/v.sql", "select {{ var('a') }}, {{ var('b', 7) }}")]);
        let out = compile(&p, &pg(), &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(out[0].sql, "select 1, 7");
        let err = compile(&p, &pg(), &serde_json::json!({})).unwrap_err();
        assert!(err.0.contains("required var \"a\" not found"), "{err}");
    }

    #[test]
    fn is_incremental_is_false() {
        let m = compile_ok(&[(
            "models/i.sql",
            "select 1{% if is_incremental() %} where false{% endif %}",
        )]);
        assert_eq!(m["i"].sql, "select 1");
    }

    #[test]
    fn rejects_unknown_refs_and_unsupported_config() {
        let p = project(&[("models/a.sql", "select * from {{ ref('nope') }}")]);
        let err = compile(&p, &pg(), &serde_json::json!({})).unwrap_err();
        assert!(
            err.0.contains("refs \"nope\", which is not a model"),
            "{err}"
        );

        let p = project(&[(
            "models/a.sql",
            "{{ config(materialized='incremental') }}select 1",
        )]);
        let err = compile(&p, &pg(), &serde_json::json!({})).unwrap_err();
        assert!(
            err.0
                .contains("materialized=\"incremental\" is not supported"),
            "{err}"
        );

        let p = project(&[("models/a.sql", "{{ config(schema='other') }}select 1")]);
        let err = compile(&p, &pg(), &serde_json::json!({})).unwrap_err();
        assert!(
            err.0.contains("config key \"schema\" is not supported"),
            "{err}"
        );
    }

    #[test]
    fn disabled_models_are_skipped_but_cannot_be_referenced() {
        let m = compile_ok(&[
            ("models/off.sql", "{{ config(enabled=false) }}select 1"),
            ("models/on.sql", "select 2"),
        ]);
        assert!(!m.contains_key("off") && m.contains_key("on"));

        let p = project(&[
            ("models/off.sql", "{{ config(enabled=false) }}select 1"),
            ("models/on.sql", "select * from {{ ref('off') }}"),
        ]);
        let err = compile(&p, &pg(), &serde_json::json!({})).unwrap_err();
        assert!(err.0.contains("which is disabled"), "{err}");
    }

    #[test]
    fn duplicate_macros_are_rejected() {
        let p = project(&[
            ("macros/a.sql", "{% macro m() %}a{% endmacro %}"),
            ("macros/b.sql", "{% macro m() %}b{% endmacro %}"),
            ("models/x.sql", "select 1"),
        ]);
        let err = compile(&p, &pg(), &serde_json::json!({})).unwrap_err();
        assert!(err.0.contains("macro \"m\" is defined in both"), "{err}");
    }

    #[test]
    fn errors_point_at_the_model_line() {
        let p = project(&[
            ("macros/a.sql", "{% macro m() %}a{% endmacro %}"),
            ("models/x.sql", "select 1\nfrom t\nwhere {{ oops( }}"),
        ]);
        let err = compile(&p, &pg(), &serde_json::json!({})).unwrap_err();
        assert!(err.0.contains("models/x.sql:3"), "{err}");
    }

    #[test]
    fn sqlite_relations_are_unqualified() {
        let p = project(&[
            ("models/a.sql", "select 1"),
            ("models/b.sql", "select * from {{ ref('a') }}"),
        ]);
        let target = Target {
            dialect: Dialect::Sqlite,
            schema: "main".into(),
        };
        let out = compile(&p, &target, &serde_json::json!({})).unwrap();
        assert_eq!(out[1].sql, r#"select * from "a""#);
    }
}
