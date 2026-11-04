//! Relation objects: what `ref()`, `source()` and `this` return.

use std::fmt;
use std::sync::Arc;

use minijinja::Value;
use minijinja::value::Object;

use crate::Dialect;

/// A relation in the database, rendered as its quoted name. Like dbt's
/// Relation, it also exposes `.schema`, `.identifier` (alias `.name`) and
/// `.database`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    pub database: String,
    pub schema: String,
    pub identifier: String,
    /// How the relation is written in SQL.
    pub rendered: String,
}

impl Relation {
    pub fn new(dialect: Dialect, database: &str, schema: &str, identifier: &str) -> Self {
        Relation {
            database: database.to_owned(),
            schema: schema.to_owned(),
            identifier: identifier.to_owned(),
            rendered: dialect.relation(schema, identifier),
        }
    }

    /// An ephemeral model, which is inlined as a CTE of this name.
    pub fn cte(dialect: Dialect, name: &str) -> Self {
        let identifier = format!("__dbt__cte__{name}");
        Relation {
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
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        Some(Value::from(match key.as_str()? {
            "database" => self.database.as_str(),
            "schema" => self.schema.as_str(),
            "identifier" | "name" | "table" => self.identifier.as_str(),
            "is_table" | "is_view" => return Some(Value::from(false)),
            _ => return None,
        }))
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.rendered)
    }
}
