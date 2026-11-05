//! Relation objects: what `ref()`, `source()` and `this` return.

use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use minijinja::listener::RenderingEventListener;
use minijinja::value::{Enumerator, Object, ObjectRepr};
use minijinja::{Error, ErrorKind, State, Value};

use crate::Dialect;

/// A relation in the database, rendered as its quoted name. Like dbt's
/// Relation, it also exposes `.schema`, `.identifier` (alias `.name`) and
/// `.database`, and is a mapping whose `metadata.type` ends in `Relation`,
/// which is how packages such as dbt_utils recognise relations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    pub dialect: Dialect,
    pub database: String,
    pub schema: String,
    pub identifier: String,
    /// How the relation is written in SQL.
    pub rendered: String,
    /// An ephemeral model, inlined as a CTE.
    pub is_cte: bool,
}

const KEYS: &[&str] = &[
    "database",
    "schema",
    "identifier",
    "name",
    "is_cte",
    "metadata",
];

impl Relation {
    /// What dbt names the CTE of an ephemeral model.
    pub const CTE_PREFIX: &str = "__dbt__cte__";

    pub fn new(dialect: Dialect, database: &str, schema: &str, identifier: &str) -> Self {
        Relation {
            dialect,
            is_cte: false,
            database: database.to_owned(),
            schema: schema.to_owned(),
            identifier: identifier.to_owned(),
            rendered: dialect.relation(schema, identifier),
        }
    }

    /// An ephemeral model, which is inlined as a CTE of this name.
    pub fn cte(dialect: Dialect, name: &str) -> Self {
        let identifier = format!("{}{name}", Self::CTE_PREFIX);
        Relation {
            dialect,
            is_cte: true,
            database: String::new(),
            schema: String::new(),
            rendered: dialect.quote(&identifier),
            identifier,
        }
    }

    pub fn into_value(self) -> Value {
        Value::from_object(self)
    }
}

impl Object for Relation {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Str(KEYS)
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        Some(Value::from(match key.as_str()? {
            "database" => self.database.as_str(),
            "schema" => self.schema.as_str(),
            "identifier" | "name" | "table" => self.identifier.as_str(),
            "is_cte" => return Some(Value::from(self.is_cte)),
            "is_table" | "is_view" | "is_materialized_view" => return Some(Value::from(false)),
            "metadata" => {
                let kind = match self.dialect {
                    Dialect::Postgres => "PostgresRelation",
                    Dialect::Sqlite => "SQLiteRelation",
                    Dialect::DuckDB => "DuckDBRelation",
                };
                return Some(Value::from_serialize(serde_json::json!({ "type": kind })));
            }
            _ => return None,
        }))
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        method: &str,
        args: &[Value],
        _listeners: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        match method {
            "get" => {
                let key = args.first().cloned().unwrap_or_default();
                Ok(self
                    .get_value(&key)
                    .or_else(|| args.get(1).cloned())
                    .unwrap_or_default())
            }
            "render" => Ok(Value::from(self.rendered.as_str())),
            // dbtindb builds in one database, so relations are always
            // written without it and these change nothing.
            "include" | "quote" => Ok(Value::from_object((**self).clone())),
            // `incorporate(path={"identifier": ...})`: the same relation
            // with parts of its path replaced.
            "incorporate" => {
                let path = args
                    .last()
                    .filter(|a| a.is_kwargs())
                    .and_then(|kw| kw.get_item(&Value::from("path")).ok())
                    .unwrap_or_default();
                let part = |name: &str, current: &str| match path.get_item(&Value::from(name)) {
                    Ok(v) if !v.is_undefined() && !v.is_none() => v.to_string(),
                    _ => current.to_owned(),
                };
                if self.is_cte {
                    return Ok(Value::from_object((**self).clone()));
                }
                Ok(Relation::new(
                    self.dialect,
                    &self.database,
                    &part("schema", &self.schema),
                    &part("identifier", &self.identifier),
                )
                .into_value())
            }
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.rendered)
    }
}
