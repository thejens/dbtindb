//! The context functions that look names up: `ref`, `source`, `doc`,
//! `var` and `env_var`. While a unit test compiles, they answer from its
//! fixtures and overrides.

use minijinja::constants::TARGET_PACKAGE_NAME;
use minijinja::{Error, State, Value};

use super::objects::Args;
use super::{Relation, Shared, jinja_error};

pub(super) fn ref_fn(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    if a.kwarg_names().iter().any(|k| k == "v" || k == "version") {
        return Err(jinja_error("ref() with a model version is not supported"));
    }
    let (package, name) = match a.positional {
        [name] => (None, name),
        [package, name] => (
            Some(
                package
                    .as_str()
                    .ok_or_else(|| jinja_error("ref(): the package must be a string"))?,
            ),
            name,
        ),
        _ => {
            return Err(jinja_error(
                "ref() takes the name of a model, seed or snapshot, and optionally its package first",
            ));
        }
    };
    let name = name
        .as_str()
        .ok_or_else(|| jinja_error("ref(): the name must be a string"))?;
    let target = s.resolve_ref(package, name, &s.current_package());
    let mut cur = s.current.lock().unwrap();
    let target = target.map_err(|e| jinja_error(format!("{} refs {name:?}: {e}", cur.node)))?;
    if !target.enabled {
        return Err(jinja_error(format!(
            "{} refs {name:?}, which is disabled (enabled=false)",
            cur.node
        )));
    }
    if let Some(parsed) = &cur.parsed_refs
        && !parsed.contains(&target.unique_id)
    {
        return Err(jinja_error(format!(
            "{} refs {name:?} only when executing, so dbtindb cannot order it; \
                 add `-- depends_on: {{{{ ref('{name}') }}}}` to the file",
            cur.node
        )));
    }
    cur.refs.insert(target.unique_id.clone());
    if let Some(fixtures) = &cur.fixtures {
        return match fixtures.refs.get(&target.unique_id) {
            Some(cte) => Ok(cte.clone().into_value()),
            None => Err(jinja_error(format!(
                "{} refs {name:?}, which the unit test does not give; add it to given",
                cur.node
            ))),
        };
    }
    if target.ephemeral && cur.execute {
        if !cur.ctes.contains(&target.unique_id) {
            cur.ctes.push(target.unique_id.clone());
        }
        return Ok(Relation::cte(s.dialect, name).into_value());
    }
    Ok(target.relation.into_value())
}

pub(super) fn source_fn(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let source = a.string(0, "source_name")?;
    let table = a.string(1, "table_name")?;
    let key = (source.clone(), table.clone());
    // Without a property file declaring it, the source name is the schema.
    let (schema, identifier) = match s.sources.get(&key) {
        Some(t) => (t.schema.clone(), t.identifier.clone()),
        None => (source, table),
    };
    let mut cur = s.current.lock().unwrap();
    cur.sources.insert(key.clone());
    if let Some(fixtures) = &cur.fixtures {
        return match fixtures.sources.get(&key) {
            Some(cte) => Ok(cte.clone().into_value()),
            None => Err(jinja_error(format!(
                "{} reads source {:?}.{:?}, which the unit test does not give; add it to given",
                cur.node, key.0, key.1
            ))),
        };
    }
    Ok(s.relation(&schema, &identifier).into_value())
}

/// `doc('name')`, or `doc('<package>', 'name')`: a docs block's text.
///
/// As in dbt, a bare name is looked up in the root project, then in the
/// package being rendered, then in the one package that has it.
pub(super) fn doc_fn(s: &Shared, state: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let (package, name) = match a.positional {
        [name] => (None, name),
        [package, name] => (package.as_str(), name),
        _ => return Err(jinja_error("doc() takes the name of a docs block")),
    };
    let name = name
        .as_str()
        .ok_or_else(|| jinja_error("doc(): the name must be a string"))?;
    let in_package = |pkg: &str| s.docs.get(pkg).and_then(|d| d.get(name));
    let text = match package {
        Some(package) => in_package(package),
        None => {
            let current = state
                .lookup(TARGET_PACKAGE_NAME, &[])
                .and_then(|v| v.as_str().map(str::to_owned));
            let mut anywhere = s.docs.values().filter_map(|d| d.get(name));
            in_package(&s.project_name)
                .or_else(|| current.as_deref().and_then(in_package))
                .or_else(|| match (anywhere.next(), anywhere.next()) {
                    (Some(text), None) => Some(text),
                    _ => None,
                })
        }
    };
    text.map(|t| Value::from(t.as_str())).ok_or_else(|| {
        jinja_error(match package {
            Some(p) => format!("doc(): package {p:?} has no docs block named {name:?}"),
            None => format!(
                "doc(): there is no docs block named {name:?}, or several packages have one; \
                 name the package: doc('<package>', {name:?})"
            ),
        })
    })
}

pub(super) fn var_fn(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let name = a.string(0, "name")?;
    let fixed = s
        .current
        .lock()
        .unwrap()
        .fixtures
        .as_ref()
        .and_then(|f| f.vars.get(&name).cloned());
    match (fixed.as_ref().or(s.vars.get(&name)), a.get(1, "default")) {
        (Some(v), _) => Ok(Value::from_serialize(v)),
        (None, Some(d)) => Ok(d),
        (None, None) => Err(jinja_error(format!(
            "required var {name:?} not found; pass it in the vars argument or set it in dbt_project.yml"
        ))),
    }
}

/// The database server's environment is not the project's to read; vars
/// take its place. A default is honoured so projects still compile.
pub(super) fn env_var_fn(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let name = a.string(0, "var")?;
    // A unit test's `overrides: {env_vars: ...}`.
    let fixed = s
        .current
        .lock()
        .unwrap()
        .fixtures
        .as_ref()
        .and_then(|f| f.env_vars.get(&name).cloned());
    if let Some(v) = fixed {
        return Ok(Value::from_serialize(v));
    }
    a.get(1, "default").ok_or_else(|| {
        jinja_error(format!(
            "env_var({name:?}): environment variables are not available inside the database; use var() instead"
        ))
    })
}
