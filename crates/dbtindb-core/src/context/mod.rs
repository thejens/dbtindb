//! dbt's Jinja context: the environment every model, macro, test, hook and
//! snapshot renders in.
//!
//! One [`Shared`] value holds the run's state. Jinja functions are
//! `'static`, so they reach the node being rendered through
//! [`Shared::current`], which the parser and the runner set before each
//! render.

mod api;
mod builtins;
mod dbt_macros;
mod functions;
mod objects;
mod packages;
mod relation;
mod statements;
mod table;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};

use md5::{Digest, Md5};
use minijinja::constants;
use minijinja::value::ValueMap;
use minijinja::{Environment, Error, ErrorKind, State, Value};
use serde_json::{Map, Value as Json};

pub use objects::to_jinja;
pub use packages::resolve as resolve_macro;
pub use relation::Relation;

use self::objects::{
    Adapter, Args, ConfigObject, Exceptions, Execute, Impl, ModelObject, NativeFn, This,
};
use crate::project::{Project, SourceTable};
use crate::{Config, Database, Dialect};

/// Per-render instruction budget, so a runaway macro cannot pin a database
/// backend: rendering never returns control to the database until it ends.
const FUEL: u64 = 50_000_000;

/// What `ref('<name>')` resolves to.
#[derive(Clone, Debug)]
pub struct RefTarget {
    pub unique_id: String,
    pub relation: Relation,
    pub ephemeral: bool,
    pub enabled: bool,
}

/// The node being rendered, and what its rendering recorded.
#[derive(Default)]
pub struct Current {
    /// False while parsing (dbt's `execute`), true while building.
    pub execute: bool,
    pub node: String,
    pub this: Option<Relation>,
    pub model: Map<String, Json>,
    /// The node's config: collected by `config()` while parsing, final
    /// while executing (what `config.get()` reads).
    pub config: Config,
    pub refs: BTreeSet<String>,
    pub sources: BTreeSet<(String, String)>,
    pub is_incremental: bool,
    /// Ephemeral models `ref()`ed while executing, in order of first use.
    pub ctes: Vec<String>,
    /// While executing: the refs found while parsing. A ref outside them
    /// would be built out of order.
    pub parsed_refs: Option<BTreeSet<String>>,
    /// What `store_result()` kept, for `load_result()`.
    pub results: BTreeMap<String, Value>,
    /// While compiling a unit test: what stands in for the model's inputs.
    pub fixtures: Option<Fixtures>,
}

/// A unit test's stand-ins: CTEs for the inputs a model reads, and the
/// vars and environment it sees.
#[derive(Clone, Debug, Default)]
pub struct Fixtures {
    /// By the unique id of the node `ref()` names.
    pub refs: BTreeMap<String, Relation>,
    pub sources: BTreeMap<(String, String), Relation>,
    pub vars: Map<String, Json>,
    pub env_vars: Map<String, Json>,
}

/// The command-line flags of the invocation, as `flags` shows them.
#[derive(Clone, Copy, Debug)]
pub struct InvocationFlags {
    /// The command: `run`, `build`, `compile`, ...
    pub which: &'static str,
    pub full_refresh: bool,
}

impl Default for InvocationFlags {
    fn default() -> Self {
        InvocationFlags {
            which: "compile",
            full_refresh: false,
        }
    }
}

pub struct Shared {
    pub dialect: Dialect,
    pub project_name: String,
    pub target_schema: String,
    pub database_name: String,
    pub vars: Map<String, Json>,
    pub db: Option<Arc<dyn Database>>,
    pub names: RwLock<BTreeMap<String, RefTarget>>,
    pub sources: BTreeMap<(String, String), SourceTable>,
    /// `dispatch:` search orders, by macro namespace.
    pub dispatch: BTreeMap<String, Vec<String>>,
    /// The installed packages' names.
    pub packages: BTreeSet<String>,
    /// `{% docs %}` blocks, for `doc()`.
    pub docs: BTreeMap<String, String>,
    pub flags: Mutex<InvocationFlags>,
    pub current: Mutex<Current>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<dbtindb context>")
    }
}

impl Shared {
    pub fn relation(&self, schema: &str, identifier: &str) -> Relation {
        Relation::new(self.dialect, &self.database_name, schema, identifier)
    }

    /// The database, but only while executing: dbt runs no queries while
    /// it parses.
    pub fn db_when_executing(&self) -> Option<&Arc<dyn Database>> {
        if self.current.lock().unwrap().execute {
            self.db.as_ref()
        } else {
            None
        }
    }

    pub fn log(&self, msg: &str) {
        if let Some(db) = self.db_when_executing() {
            db.log(msg);
        }
    }

    /// Starts rendering `node`, clearing what the previous render recorded.
    pub fn begin(
        &self,
        execute: bool,
        node: &str,
        this: Option<Relation>,
        model: Map<String, Json>,
        config: Config,
    ) {
        *self.current.lock().unwrap() = Current {
            execute,
            node: node.to_owned(),
            this,
            model,
            config,
            ..Current::default()
        };
    }

    /// Ends a render, returning what it recorded.
    pub fn finish(&self) -> Current {
        std::mem::take(&mut *self.current.lock().unwrap())
    }
}

/// Builds the environment for `project`, with its macros registered in
/// dbt's global macro namespace.
pub fn environment(
    project: &Project,
    shared: Arc<Shared>,
) -> Result<Environment<'static>, crate::Error> {
    let mut env = Environment::new();
    env.set_fuel(Some(FUEL));
    let files = Arc::new(project.files.clone());
    env.set_loader(move |name| Ok(files.get(name).cloned()));

    // The fork resolves an unknown name `m` as the template `<pkg>.m`:
    // first in the current package, then in this project, then in dbt's.
    let pkg = shared.project_name.clone();
    env.add_global(constants::ROOT_PACKAGE_NAME, Value::from(pkg.as_str()));
    env.add_global(constants::TARGET_PACKAGE_NAME, Value::from(pkg.as_str()));
    env.add_global(constants::DIALECT, Value::from(shared.dialect.name()));

    let mut registry = packages::Registry::default();
    registry.add_file(
        &mut env,
        packages::DBT,
        builtins::BUILTINS_PATH,
        builtins::BUILTINS,
        false,
    )?;
    for (path, source) in dbt_macros::files(shared.dialect) {
        registry.add_file(
            &mut env,
            packages::DBT,
            &format!("<dbt>/{path}"),
            source,
            false,
        )?;
    }
    for path in &project.macro_files {
        registry.add_file(&mut env, &pkg, path, &project.files[path], false)?;
    }
    for package in project.packages.values() {
        for path in &package.macro_files {
            registry.add_file(&mut env, &package.name, path, &project.files[path], true)?;
        }
    }

    let mut dbt_names = ValueMap::new();
    for name in registry.macros.get(packages::DBT).into_iter().flatten() {
        dbt_names.insert(Value::from(name.as_str()), Value::from(packages::DBT));
    }
    let mut namespaces = ValueMap::new();
    namespaces.insert(
        Value::from(shared.dialect.name()),
        Value::from_object(dbt_names),
    );
    env.add_global(
        constants::DBT_AND_ADAPTERS_NAMESPACE,
        Value::from_object(namespaces),
    );

    // `modules` (datetime, re, pytz), `dict`, `cycler` and `joiner`, and
    // the Python methods dbt's Jinja has: `d.get(k, default)`, `s.split()`.
    minijinja_contrib::add_to_environment(&mut env);
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);

    env.add_global(
        "target",
        Value::from_serialize(serde_json::json!({
            "name": "default",
            "type": shared.dialect.name(),
            "schema": shared.target_schema,
            "database": shared.database_name,
            "profile_name": pkg,
            "threads": 1,
        })),
    );
    env.add_global("project_name", Value::from(pkg.as_str()));
    env.add_global("config", Value::from_object(ConfigObject(shared.clone())));
    env.add_global("adapter", Value::from_object(Adapter(shared.clone())));
    env.add_global("exceptions", Value::from_object(Exceptions(shared.clone())));
    env.add_global("this", Value::from_object(This(shared.clone())));
    env.add_global("execute", Value::from_object(Execute(shared.clone())));
    env.add_global("model", Value::from_object(ModelObject(shared.clone())));
    env.add_global("api", Value::from_object(api::Api(shared.clone())));
    env.add_global(
        "flags",
        Value::from_object(functions::Flags(shared.clone())),
    );
    // The dbt release whose behaviour dbtindb follows. Macros test it: dbt's
    // own `equals` takes a dbt 2-only path for versions starting with `2.`.
    env.add_global("dbt_version", Value::from("1.10.0"));
    env.add_global("run_started_at", functions::run_started_at());
    env.add_global("invocation_id", functions::invocation_id());

    let functions: [(&'static str, Impl); 13] = [
        ("ref", ref_fn),
        ("source", source_fn),
        ("doc", doc_fn),
        ("var", var_fn),
        ("env_var", env_var_fn),
        ("is_incremental", |s, _, _| {
            Ok(Value::from(s.current.lock().unwrap().is_incremental))
        }),
        ("run_query", run_query_fn),
        ("load_result", statements::load_result),
        ("load_relation", statements::load_relation),
        ("log", log_fn),
        ("fromjson", |_, _, a| {
            let text = a.string(0, "string")?;
            serde_json::from_str::<Json>(&text)
                .map(|v| Value::from_serialize(&v))
                .map_err(|e| jinja_error(format!("fromjson(): {e}")))
        }),
        ("tojson", |_, _, a| {
            let v = a.get(0, "value").unwrap_or_default();
            Ok(Value::from(
                serde_json::to_string(&to_json(&v)?).unwrap_or_default(),
            ))
        }),
        ("local_md5", |_, _, a| {
            let digest = Md5::digest(a.string(0, "value")?.as_bytes());
            Ok(Value::from(
                digest
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
            ))
        }),
    ];
    for (name, f) in functions.into_iter().chain(functions::FUNCTIONS) {
        env.add_global(
            name,
            Value::from_object(NativeFn {
                shared: shared.clone(),
                name,
                f,
            }),
        );
    }
    // Last, so a package named like a context variable is an error.
    registry.add_namespaces(&mut env)?;
    Ok(env)
}

/// The macros (and `{% test %}`/`{% snapshot %}` blocks) a file defines.
pub fn macro_names(
    env: &mut Environment<'static>,
    path: &str,
    source: &str,
) -> Result<Vec<String>, crate::Error> {
    env.add_template_owned(path.to_owned(), source.to_owned(), Some(path.to_owned()))?;
    let template = env.get_template(path)?;
    let state = template
        .eval_to_state((), &[])
        .map_err(|e| crate::Error::new(format!("loading macros from {path}: {e}")))?;
    let mut names: Vec<String> = state
        .exports()
        .into_iter()
        // Macro objects expose their `arguments`; a top-level {% set %}
        // value does not, and is not a macro.
        .filter(|n| {
            state
                .lookup(n, &[])
                .and_then(|v| v.get_attr("arguments").ok())
                .is_some_and(|a| !a.is_undefined())
        })
        .map(str::to_owned)
        .collect();
    names.sort();
    Ok(names)
}

fn ref_fn(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    if a.kwarg_names().iter().any(|k| k == "v" || k == "version") {
        return Err(jinja_error("ref() with a model version is not supported"));
    }
    let name = match a.positional {
        [name] => name,
        [package, name] if package.as_str() == Some(s.project_name.as_str()) => name,
        [package, _] => {
            return Err(jinja_error(format!(
                "ref() to package {package}: packages are not supported"
            )));
        }
        _ => {
            return Err(jinja_error(
                "ref() takes the name of a model, seed or snapshot",
            ));
        }
    };
    let name = name
        .as_str()
        .ok_or_else(|| jinja_error("ref(): the name must be a string"))?;
    let target = s.names.read().unwrap().get(name).cloned();
    let mut cur = s.current.lock().unwrap();
    let Some(target) = target else {
        return Err(jinja_error(format!(
            "{} refs {name:?}, which is not a model, seed or snapshot in this project",
            cur.node
        )));
    };
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

fn source_fn(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
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

/// `doc('name')`, or `doc('<project>', 'name')`: a docs block's text.
fn doc_fn(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let name = match a.positional {
        [name] => name,
        [package, name] if package.as_str() == Some(s.project_name.as_str()) => name,
        [package, _] => {
            return Err(jinja_error(format!(
                "doc(): docs blocks of package {package} are not loaded; only this project's are"
            )));
        }
        _ => return Err(jinja_error("doc() takes the name of a docs block")),
    };
    let name = name
        .as_str()
        .ok_or_else(|| jinja_error("doc(): the name must be a string"))?;
    s.docs
        .get(name)
        .map(|text| Value::from(text.as_str()))
        .ok_or_else(|| jinja_error(format!("doc(): there is no docs block named {name:?}")))
}

fn var_fn(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
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
fn env_var_fn(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
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

fn run_query_fn(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let sql = a.string(0, "sql")?;
    // As in dbt, run_query() returns None while parsing; guard uses of
    // its result with `{% if execute %}`.
    let Some(db) = s.db_when_executing() else {
        return Ok(Value::from(()));
    };
    Ok(table::TableObject::value(db.query(&sql).map_err(to_jinja)?))
}

fn log_fn(s: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let msg = a.get(0, "msg").map(|v| v.to_string()).unwrap_or_default();
    s.log(&msg);
    Ok(Value::from(""))
}

pub fn jinja_error(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidOperation, msg.into())
}

pub fn to_json(v: &Value) -> Result<Json, Error> {
    serde_json::to_value(v).map_err(|e| jinja_error(format!("cannot convert {v} to JSON: {e}")))
}
