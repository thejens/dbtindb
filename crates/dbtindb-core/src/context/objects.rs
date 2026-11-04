//! The callable objects of dbt's Jinja context: `config`, `adapter`,
//! `exceptions`, and the per-node values `this`, `model` and `execute`.

use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use minijinja::listener::RenderingEventListener;
use minijinja::value::{Enumerator, Object, ObjectRepr};
use minijinja::{Error, ErrorKind, State, Value};

use super::api::ColumnObject;
use super::relation::Relation;
use super::{Shared, jinja_error, packages, statements, to_json};

/// A Rust function exposed to Jinja with full access to its arguments
/// (positional and keyword) and to the shared state of the run.
pub type Impl = fn(&Shared, &State<'_, '_>, &Args<'_>) -> Result<Value, Error>;

pub struct NativeFn {
    pub shared: Arc<Shared>,
    pub name: &'static str,
    pub f: Impl,
}

impl fmt::Debug for NativeFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<function {}>", self.name)
    }
}

impl Object for NativeFn {
    fn call(
        self: &Arc<Self>,
        state: &State<'_, '_>,
        args: &[Value],
        _listeners: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        (self.f)(&self.shared, state, &Args::new(self.name, args))
    }
}

/// Positional and keyword arguments of a call.
pub struct Args<'a> {
    pub name: &'a str,
    pub positional: &'a [Value],
    kwargs: Option<&'a Value>,
}

impl<'a> Args<'a> {
    pub fn new(name: &'a str, args: &'a [Value]) -> Self {
        match args.split_last() {
            Some((last, rest)) if last.is_kwargs() => Args {
                name,
                positional: rest,
                kwargs: Some(last),
            },
            _ => Args {
                name,
                positional: args,
                kwargs: None,
            },
        }
    }

    /// Argument `name`, given by keyword or at position `pos`.
    pub fn get(&self, pos: usize, name: &str) -> Option<Value> {
        if let Some(kw) = self.kwargs
            && let Ok(v) = kw.get_item(&Value::from(name))
            && !v.is_undefined()
        {
            return Some(v);
        }
        self.positional.get(pos).cloned()
    }

    pub fn string(&self, pos: usize, name: &str) -> Result<String, Error> {
        match self.get(pos, name) {
            Some(v) => v
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| jinja_error(format!("{}(): {name} must be a string", self.name))),
            None => Err(jinja_error(format!(
                "{}(): missing argument {name}",
                self.name
            ))),
        }
    }

    /// The keyword arguments as a JSON object.
    pub fn kwargs_json(&self) -> Result<serde_json::Map<String, serde_json::Value>, Error> {
        match self.kwargs {
            None => Ok(Default::default()),
            Some(kw) => match to_json(kw)? {
                serde_json::Value::Object(m) => Ok(m),
                _ => Ok(Default::default()),
            },
        }
    }

    pub fn kwarg_names(&self) -> Vec<String> {
        self.kwargs_json()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }
}

/// `config`: `config(materialized='table')` while parsing, and
/// `config.get(key, default)` / `config.require(key)` anywhere.
#[derive(Debug)]
pub struct ConfigObject(pub Arc<Shared>);

impl Object for ConfigObject {
    fn call(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        args: &[Value],
        _listeners: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        let args = Args::new("config", args);
        let mut values = args.kwargs_json()?;
        for p in args.positional {
            match to_json(p)? {
                serde_json::Value::Object(m) => values.extend(m),
                _ => return Err(jinja_error("config() takes keyword arguments or a dict")),
            }
        }
        let mut cur = self.0.current.lock().unwrap();
        // Config is collected while parsing; at execution the node's config
        // is already final, as in dbt.
        if !cur.execute {
            cur.config.merge(&values);
        }
        Ok(Value::from(""))
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        method: &str,
        args: &[Value],
        _listeners: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        let args = Args::new(method, args);
        let key = args.string(0, "name")?;
        let cur = self.0.current.lock().unwrap();
        let value = cur.config.get(&key).cloned();
        match (method, value) {
            ("get", Some(v)) => Ok(Value::from_serialize(&v)),
            ("get", None) => Ok(args.get(1, "default").unwrap_or(Value::from(()))),
            ("require", Some(v)) => Ok(Value::from_serialize(&v)),
            ("require", None) => Err(jinja_error(format!(
                "config {key:?} is required by {} but not set",
                cur.node
            ))),
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

/// `exceptions.raise_compiler_error(msg)` and `exceptions.warn(msg)`.
#[derive(Debug)]
pub struct Exceptions(pub Arc<Shared>);

impl Object for Exceptions {
    fn call_method(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        method: &str,
        args: &[Value],
        _listeners: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        let args = Args::new(method, args);
        let msg = args
            .get(0, "msg")
            .map(|v| v.to_string())
            .unwrap_or_default();
        match method {
            "raise_compiler_error" | "raise_not_implemented" => Err(jinja_error(msg)),
            "warn" => {
                self.0.log(&format!("warning: {msg}"));
                Ok(Value::from(""))
            }
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

/// `adapter`: the parts of dbt's adapter interface that projects use in
/// their own macros.
#[derive(Debug)]
pub struct Adapter(pub Arc<Shared>);

impl Object for Adapter {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        match key.as_str()? {
            "behavior" => Some(Value::from_object(Behavior)),
            _ => None,
        }
    }

    fn call_method(
        self: &Arc<Self>,
        state: &State<'_, '_>,
        method: &str,
        args: &[Value],
        _listeners: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        let shared = &self.0;
        let args = Args::new(method, args);
        match method {
            "type" => Ok(Value::from(shared.dialect.name())),
            "quote" => Ok(Value::from(
                shared.dialect.quote(&args.string(0, "identifier")?),
            )),
            "dispatch" => {
                let name = args.string(0, "macro_name")?;
                let namespace = args.get(1, "macro_namespace").filter(|v| !v.is_none());
                packages::dispatch(
                    state,
                    shared.dialect.name(),
                    &shared.project_name,
                    &shared.dispatch,
                    &shared.packages,
                    &name,
                    namespace.as_ref().and_then(Value::as_str),
                )
            }
            "execute" => statements::execute(shared, &args),
            "get_relation" => {
                let schema = args.string(1, "schema")?;
                let identifier = args.string(2, "identifier")?;
                let Some(db) = shared.db_when_executing() else {
                    return Ok(Value::from(()));
                };
                Ok(
                    match db.relation_kind(&schema, &identifier).map_err(to_jinja)? {
                        Some(_) => shared.relation(&schema, &identifier).into_value(),
                        None => Value::from(()),
                    },
                )
            }
            "load_relation" => statements::load_relation(shared, state, &args),
            "get_columns_in_relation" => {
                let rel = args
                    .get(0, "relation")
                    .ok_or_else(|| jinja_error("get_columns_in_relation(): missing relation"))?;
                let Some(rel) = relation_of(&rel) else {
                    return Err(jinja_error("get_columns_in_relation() takes a relation"));
                };
                let Some(db) = shared.db_when_executing() else {
                    return Ok(Value::from(Vec::<Value>::new()));
                };
                let cols = db.columns(&rel.schema, &rel.identifier).map_err(to_jinja)?;
                let mut out = Vec::with_capacity(cols.len());
                for c in &cols {
                    let data_type = if c.data_type.is_empty() {
                        stored_type(db.as_ref(), shared, &rel, &c.name)?
                    } else {
                        c.data_type.clone()
                    };
                    out.push(ColumnObject::value(shared.dialect, &c.name, &data_type));
                }
                Ok(Value::from(out))
            }
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

/// The type of what a column holds, for a SQLite column declared without
/// one (a view's computed column): macros cast to `column.data_type`, and
/// `cast(x as )` is not SQL. A column of NULLs is TEXT, which casts NULL
/// to NULL.
fn stored_type(
    db: &dyn crate::Database,
    shared: &Shared,
    rel: &Relation,
    column: &str,
) -> Result<String, Error> {
    let d = shared.dialect;
    let col = d.quote(column);
    let sql = format!(
        "SELECT typeof({col}) FROM {} WHERE {col} IS NOT NULL LIMIT 1",
        rel.rendered
    );
    let table = db.query(&sql).map_err(to_jinja)?;
    let kind = table
        .rows
        .first()
        .and_then(|r| r.first())
        .and_then(|v| v.as_str())
        .unwrap_or("text");
    Ok(match kind {
        "integer" => "INTEGER",
        "real" => "REAL",
        "blob" => "BLOB",
        _ => "TEXT",
    }
    .to_owned())
}

/// `adapter.behavior`: dbt's behaviour-change flags. dbtindb keeps each
/// at its default, which is off for every flag the dbt macros consult.
#[derive(Debug)]
struct Behavior;

impl Object for Behavior {
    fn get_value(self: &Arc<Self>, _key: &Value) -> Option<Value> {
        Some(Value::from_object(Flag))
    }
}

/// One behaviour flag: false, whether read as `flag` or `flag.no_warn`.
#[derive(Debug)]
struct Flag;

impl Object for Flag {
    fn is_true(self: &Arc<Self>) -> bool {
        false
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        (key.as_str()? == "no_warn").then(|| Value::from(false))
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("False")
    }
}

/// The relation a value stands for: a relation, or `this`.
pub fn relation_of(v: &Value) -> Option<Relation> {
    if let Some(rel) = v.downcast_object_ref::<Relation>() {
        return Some(rel.clone());
    }
    v.downcast_object_ref::<This>()?
        .0
        .current
        .lock()
        .unwrap()
        .this
        .clone()
}

/// `this`: the relation of the node being rendered.
#[derive(Debug)]
pub struct This(pub Arc<Shared>);

impl This {
    fn current(&self) -> Option<Arc<Relation>> {
        self.0.current.lock().unwrap().this.clone().map(Arc::new)
    }
}

impl Object for This {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        match self.current() {
            Some(rel) => rel.enumerate(),
            None => Enumerator::Empty,
        }
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        self.current()?.get_value(key)
    }

    fn call_method(
        self: &Arc<Self>,
        state: &State<'_, '_>,
        method: &str,
        args: &[Value],
        listeners: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        match self.current() {
            Some(rel) => rel.call_method(state, method, args, listeners),
            None => Err(jinja_error("`this` is only defined while rendering a node")),
        }
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.current() {
            Some(rel) => f.write_str(&rel.rendered),
            None => Ok(()),
        }
    }
}

/// `execute`: false while dbtindb parses the project, true while it runs.
#[derive(Debug)]
pub struct Execute(pub Arc<Shared>);

impl Object for Execute {
    fn is_true(self: &Arc<Self>) -> bool {
        self.0.current.lock().unwrap().execute
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.is_true() { "True" } else { "False" })
    }
}

/// `model`: the node being rendered, as a mapping (`model.name`,
/// `model.unique_id`, `model.config`, ...).
#[derive(Debug)]
pub struct ModelObject(pub Arc<Shared>);

impl Object for ModelObject {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let cur = self.0.current.lock().unwrap();
        cur.model.get(key.as_str()?).map(Value::from_serialize)
    }
}

pub fn to_jinja(e: crate::Error) -> Error {
    jinja_error(e.0)
}
