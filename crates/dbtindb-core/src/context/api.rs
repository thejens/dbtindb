//! `api.Column` and `api.Relation`, and the column objects
//! `adapter.get_columns_in_relation()` returns.

use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use minijinja::listener::RenderingEventListener;
use minijinja::value::{Enumerator, Object, ObjectRepr};
use minijinja::{Error, ErrorKind, State, Value};

use super::objects::Args;
use super::relation::Relation;
use super::{Shared, jinja_error};
use crate::Dialect;

/// `api`: dbt's adapter classes, for the class methods macros call.
#[derive(Debug)]
pub struct Api(pub Arc<Shared>);

impl Object for Api {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        match key.as_str()? {
            "Column" => Some(Value::from_object(ColumnClass(self.0.dialect))),
            "Relation" => Some(Value::from_object(RelationClass(self.0.clone()))),
            _ => None,
        }
    }
}

/// `api.Column`.
#[derive(Debug)]
struct ColumnClass(Dialect);

impl Object for ColumnClass {
    fn call_method(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        method: &str,
        args: &[Value],
        _listeners: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        let args = Args::new(method, args);
        match method {
            "translate_type" => Ok(Value::from(translate_type(
                self.0,
                &args.string(0, "dtype")?,
            ))),
            // `numeric(28,6)`, or the bare type without precision and scale.
            "numeric_type" => {
                let dtype = args.string(0, "dtype")?;
                let precision = args.get(1, "precision").filter(|v| !v.is_none());
                let scale = args.get(2, "scale").filter(|v| !v.is_none());
                Ok(Value::from(match (precision, scale) {
                    (Some(p), Some(s)) => format!("{dtype}({p},{s})"),
                    _ => dtype,
                }))
            }
            "string_type" => {
                let size = args.get(0, "size").unwrap_or_default();
                Ok(Value::from(match self.0 {
                    Dialect::Postgres => format!("character varying({size})"),
                    Dialect::Sqlite => "TEXT".to_owned(),
                    // DuckDB accepts a length but does not enforce it.
                    Dialect::DuckDB => "VARCHAR".to_owned(),
                }))
            }
            "create" => Ok(ColumnObject::value(
                self.0,
                &args.string(0, "name")?,
                &args.string(1, "label_or_dtype")?,
            )),
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

/// dbt's type labels (`string`, `timestamp`, ...) as this database names
/// them; other types pass through.
pub fn translate_type(dialect: Dialect, dtype: &str) -> String {
    let label = dtype.to_ascii_lowercase();
    let translated = match (dialect, label.as_str()) {
        (Dialect::Postgres, "string") => "TEXT",
        (Dialect::Postgres, "timestamp") => "TIMESTAMP",
        (Dialect::Postgres, "float") => "FLOAT",
        (Dialect::Postgres, "integer") => "INT",
        // SQLite has no date types; a `timestamp` type name would give
        // the column NUMERIC affinity and mangle 'YYYY-MM-DD' text.
        (Dialect::Sqlite, "string" | "timestamp") => "TEXT",
        (Dialect::Sqlite, "float") => "REAL",
        (Dialect::Sqlite, "integer" | "bigint") => "INTEGER",
        (Dialect::DuckDB, "string") => "VARCHAR",
        (Dialect::DuckDB, "timestamp") => "TIMESTAMP",
        // DuckDB's FLOAT is single precision; dbt's `float` means double.
        (Dialect::DuckDB, "float") => "DOUBLE",
        (Dialect::DuckDB, "integer") => "INTEGER",
        _ => return dtype.to_owned(),
    };
    translated.to_owned()
}

/// `api.Relation`.
#[derive(Debug)]
struct RelationClass(Arc<Shared>);

impl Object for RelationClass {
    fn call_method(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        method: &str,
        args: &[Value],
        _listeners: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        let args = Args::new(method, args);
        let text = |pos, name| {
            args.get(pos, name)
                .filter(|v| !v.is_none())
                .map(|v| v.to_string())
        };
        match method {
            "create" => {
                let schema = text(1, "schema").unwrap_or_else(|| self.0.target_schema.clone());
                let identifier = text(2, "identifier")
                    .ok_or_else(|| jinja_error("api.Relation.create(): missing identifier"))?;
                Ok(self.0.relation(&schema, &identifier).into_value())
            }
            "add_ephemeral_prefix" => Ok(Value::from(format!(
                "{}{}",
                Relation::CTE_PREFIX,
                text(0, "name").unwrap_or_default()
            ))),
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

/// A column of a relation, like dbt's `Column`: `.name`, `.quoted`,
/// `.data_type`, `.is_string()`, ...
#[derive(Debug)]
pub struct ColumnObject {
    dialect: Dialect,
    name: String,
    data_type: String,
}

impl ColumnObject {
    pub fn value(dialect: Dialect, name: &str, data_type: &str) -> Value {
        Value::from_object(ColumnObject {
            dialect,
            name: name.to_owned(),
            data_type: data_type.to_owned(),
        })
    }

    /// The type without its size or precision: `character varying(10)` is
    /// `character varying`.
    fn base_type(&self) -> String {
        let base = self.data_type.split('(').next().unwrap_or_default();
        base.trim().to_ascii_lowercase()
    }

    fn is(&self, kinds: &[&str]) -> bool {
        kinds.contains(&self.base_type().as_str())
    }

    fn is_string(&self) -> bool {
        self.is(&[
            "text",
            "character varying",
            "character",
            "varchar",
            "char",
            "clob",
            "string",
        ])
    }

    fn is_integer(&self) -> bool {
        self.is(&[
            "smallint",
            "integer",
            "bigint",
            "int",
            "int2",
            "int4",
            "int8",
            "smallserial",
            "serial",
            "bigserial",
            "serial2",
            "serial4",
            "serial8",
        ])
    }

    fn is_float(&self) -> bool {
        self.is(&[
            "real",
            "float",
            "float4",
            "float8",
            "double",
            "double precision",
        ])
    }

    fn is_numeric(&self) -> bool {
        self.is(&["numeric", "decimal"])
    }

    /// The size of a string type, as dbt's `string_size()`.
    fn string_size(&self) -> Option<i64> {
        let (_, rest) = self.data_type.split_once('(')?;
        rest.trim_end_matches(')').trim().parse().ok()
    }
}

const COLUMN_KEYS: &[&str] = &["name", "column", "data_type", "dtype", "quoted"];

impl Object for ColumnObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Str(COLUMN_KEYS)
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        Some(match key.as_str()? {
            "name" | "column" => Value::from(self.name.as_str()),
            "data_type" | "dtype" => Value::from(self.data_type.as_str()),
            "quoted" => Value::from(self.dialect.quote(&self.name)),
            "char_size" => Value::from(self.string_size()),
            _ => return None,
        })
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        method: &str,
        args: &[Value],
        _listeners: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        Ok(Value::from(match method {
            "is_string" => self.is_string(),
            "is_integer" => self.is_integer(),
            "is_float" => self.is_float(),
            "is_numeric" => self.is_numeric(),
            "is_number" => self.is_integer() || self.is_float() || self.is_numeric(),
            "string_size" => return Ok(Value::from(self.string_size())),
            "get" => {
                let args = Args::new(method, args);
                let key = args.get(0, "key").unwrap_or_default();
                return Ok(self
                    .get_value(&key)
                    .or_else(|| args.get(1, "default"))
                    .unwrap_or_default());
            }
            _ => return Err(Error::from(ErrorKind::UnknownMethod)),
        }))
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<Column {} ({})>", self.name, self.data_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_type_labels() {
        assert_eq!(translate_type(Dialect::Postgres, "string"), "TEXT");
        assert_eq!(translate_type(Dialect::Postgres, "bigint"), "bigint");
        assert_eq!(translate_type(Dialect::Sqlite, "timestamp"), "TEXT");
        assert_eq!(translate_type(Dialect::Sqlite, "float"), "REAL");
    }

    #[test]
    fn classifies_column_types() {
        let col = |t: &str| ColumnObject {
            dialect: Dialect::Postgres,
            name: "c".into(),
            data_type: t.into(),
        };
        assert!(col("character varying(10)").is_string());
        assert_eq!(col("character varying(10)").string_size(), Some(10));
        assert!(col("INTEGER").is_integer());
        assert!(col("double precision").is_float());
        assert!(col("numeric(28,6)").is_numeric());
        assert!(!col("date").is_string());
    }
}
