//! A unit test's `overrides: {macros: ...}`: stand-ins for macros that
//! return a fixed value instead of running.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use minijinja::listener::RenderingEventListener;
use minijinja::value::{Enumerator, Object};
use minijinja::{Error, State, Value};
use serde_json::Value as Json;

/// A macro that returns `value`, whatever it is called with.
#[derive(Debug)]
struct Fixed(Value);

impl Object for Fixed {
    fn call(
        self: &Arc<Self>,
        _: &State<'_, '_>,
        _: &[Value],
        _: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        Ok(self.0.clone())
    }
}

/// The stand-in for an overridden macro.
pub fn overriding(value: &Json) -> Value {
    Value::from_object(Fixed(Value::from_serialize(value)))
}

/// A package namespace (`dbt_utils`) whose `overrides` replace its
/// macros of those names; its other macros are the package's own.
#[derive(Debug)]
struct Overridden {
    original: Option<Value>,
    overrides: BTreeMap<String, Value>,
}

impl Object for Overridden {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let name = key.as_str()?;
        if let Some(v) = self.overrides.get(name) {
            return Some(v.clone());
        }
        self.original
            .as_ref()
            .and_then(|o| o.get_attr(name).ok())
            .filter(|v| !v.is_undefined())
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        let mut names: Vec<Value> = self
            .original
            .as_ref()
            .and_then(|o| o.try_iter().ok())
            .map(|it| it.collect())
            .unwrap_or_default();
        for name in self.overrides.keys() {
            let name = Value::from(name.as_str());
            if !names.contains(&name) {
                names.push(name);
            }
        }
        Enumerator::Values(names)
    }
}

/// `original`, the namespace of a package, with `overrides` in place of
/// the macros they name.
pub fn overridden_namespace(original: Option<Value>, overrides: BTreeMap<String, Value>) -> Value {
    Value::from_object(Overridden {
        original,
        overrides,
    })
}
