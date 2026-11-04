//! The result of `run_query()`, shaped like the agate tables dbt returns:
//! `table.rows`, `table.columns[0].values()`, `table.column_names`, and
//! iteration over rows.

use std::sync::Arc;

use minijinja::listener::RenderingEventListener;
use minijinja::value::{Enumerator, Object, ObjectRepr};
use minijinja::{Error, ErrorKind, State, Value};
use std::rc::Rc;

use crate::Table;

#[derive(Debug)]
pub struct TableObject {
    columns: Arc<Vec<String>>,
    rows: Vec<Value>,
    by_column: Vec<Vec<Value>>,
}

impl TableObject {
    /// The Jinja value for a query result.
    pub fn value(table: Table) -> Value {
        let columns = Arc::new(table.columns);
        let mut by_column = vec![Vec::new(); columns.len()];
        let rows = table
            .rows
            .into_iter()
            .map(|row| {
                let values: Vec<Value> = row.into_iter().map(Value::from_serialize).collect();
                for (i, v) in values.iter().enumerate() {
                    if let Some(col) = by_column.get_mut(i) {
                        col.push(v.clone());
                    }
                }
                Value::from_object(Row {
                    columns: columns.clone(),
                    values,
                })
            })
            .collect();
        Value::from_object(TableObject {
            columns,
            rows,
            by_column,
        })
    }
}

impl Object for TableObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Seq(self.rows.len())
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        if let Some(i) = key.as_usize() {
            return self.rows.get(i).cloned();
        }
        match key.as_str()? {
            "rows" => Some(Value::from(self.rows.clone())),
            "column_names" => Some(Value::from_serialize(&*self.columns)),
            "columns" => Some(Value::from(
                self.columns
                    .iter()
                    .zip(&self.by_column)
                    .map(|(name, values)| {
                        Value::from_object(ColumnObject {
                            name: name.clone(),
                            values: values.clone(),
                        })
                    })
                    .collect::<Vec<_>>(),
            )),
            _ => None,
        }
    }
}

/// One row: indexable by position or by column name.
#[derive(Debug)]
struct Row {
    columns: Arc<Vec<String>>,
    values: Vec<Value>,
}

impl Object for Row {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Seq(self.values.len())
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        if let Some(i) = key.as_usize() {
            return self.values.get(i).cloned();
        }
        let name = key.as_str()?;
        let i = self.columns.iter().position(|c| c == name)?;
        self.values.get(i).cloned()
    }
}

#[derive(Debug)]
struct ColumnObject {
    name: String,
    values: Vec<Value>,
}

impl Object for ColumnObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Seq(self.values.len())
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        if let Some(i) = key.as_usize() {
            return self.values.get(i).cloned();
        }
        match key.as_str()? {
            "name" => Some(Value::from(self.name.as_str())),
            _ => None,
        }
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        method: &str,
        _args: &[Value],
        _listeners: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        match method {
            "values" => Ok(Value::from(self.values.clone())),
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}
