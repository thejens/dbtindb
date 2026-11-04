//! dbt's Python-flavoured context functions: `zip`, `set`, `fromyaml`,
//! `toyaml`, `print`, and the per-invocation values `run_started_at`,
//! `invocation_id` and `flags`.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::rc::Rc;
use std::sync::Arc;

use chrono_tz::Tz;
use minijinja::listener::RenderingEventListener;
use minijinja::value::{Enumerator, Object, ObjectRepr};
use minijinja::{Error, ErrorKind, State, Value};
use minijinja_contrib::modules::py_datetime::datetime::PyDateTime;
use minijinja_contrib::modules::pytz::PytzTimezone;
use serde_json::Value as Json;

use super::objects::{Args, Impl};
use super::{Shared, jinja_error, to_json};

/// The functions, by name.
pub const FUNCTIONS: [(&str, Impl); 13] = [
    ("zip", |_, _, a| zip(a).or(Ok(Value::from(())))),
    ("zip_strict", |_, _, a| zip(a)),
    ("set", |_, _, a| set(a).or(Ok(Value::from(())))),
    ("set_strict", |_, _, a| set(a)),
    ("fromyaml", fromyaml),
    ("toyaml", toyaml),
    ("print", |s, _, a| {
        s.log(&a.get(0, "msg").map(|v| v.to_string()).unwrap_or_default());
        Ok(Value::from(""))
    }),
    // dbt writes a node's compiled SQL to its target directory; there is
    // none inside the database.
    ("write", |_, _, _| Ok(Value::from(""))),
    ("statement", super::statements::statement),
    ("noop_statement", super::statements::noop_statement),
    ("store_result", super::statements::store_result),
    ("store_raw_result", super::statements::store_raw_result),
    ("try_or_compiler_error", try_or_compiler_error),
];

/// `zip(a, b, ...)`: tuples of the items at each position, as long as the
/// shortest argument.
fn zip(a: &Args<'_>) -> Result<Value, Error> {
    let lists = a
        .positional
        .iter()
        .map(|v| v.try_iter().map(|it| it.collect::<Vec<_>>()))
        .collect::<Result<Vec<_>, _>>()?;
    let len = lists.iter().map(Vec::len).min().unwrap_or(0);
    Ok(Value::from(
        (0..len)
            .map(|i| Value::from(lists.iter().map(|l| l[i].clone()).collect::<Vec<_>>()))
            .collect::<Vec<_>>(),
    ))
}

/// `set(iterable)`: the distinct items.
fn set(a: &Args<'_>) -> Result<Value, Error> {
    let items = match a.get(0, "value") {
        Some(v) => v.try_iter()?.collect(),
        None => Vec::new(),
    };
    Ok(SetObject::value(items))
}

/// A Python set. Its items are kept sorted, so two sets with the same
/// items compare equal, as Python's do, whatever order they were built in.
#[derive(Debug)]
struct SetObject(Vec<Value>);

impl SetObject {
    fn new(mut items: Vec<Value>) -> Self {
        items.sort();
        items.dedup();
        SetObject(items)
    }

    fn value(items: Vec<Value>) -> Value {
        Value::from_object(Self::new(items))
    }

    fn contains(&self, v: &Value) -> bool {
        self.0.binary_search(v).is_ok()
    }
}

impl Object for SetObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Seq(self.0.len())
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        self.0.get(key.as_usize()?).cloned()
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        method: &str,
        args: &[Value],
        _listeners: &[Rc<dyn RenderingEventListener>],
    ) -> Result<Value, Error> {
        let other: Vec<Value> = match args.first() {
            Some(v) => v.try_iter()?.collect(),
            None => Vec::new(),
        };
        let other_set = SetObject::new(other.clone());
        let mine = self.0.iter().cloned();
        Ok(match method {
            "union" => SetObject::value(mine.chain(other).collect()),
            "intersection" => SetObject::value(mine.filter(|v| other_set.contains(v)).collect()),
            "difference" => SetObject::value(mine.filter(|v| !other_set.contains(v)).collect()),
            "symmetric_difference" => SetObject::value(
                mine.filter(|v| !other_set.contains(v))
                    .chain(other.into_iter().filter(|v| !self.contains(v)))
                    .collect(),
            ),
            "issubset" => Value::from(self.0.iter().all(|v| other_set.contains(v))),
            "issuperset" => Value::from(other.iter().all(|v| self.contains(v))),
            "isdisjoint" => Value::from(!self.0.iter().any(|v| other_set.contains(v))),
            _ => return Err(Error::from(ErrorKind::UnknownMethod)),
        })
    }
}

/// `try_or_compiler_error(message, f, *args)`: `f(*args)`, or a compiler
/// error with `message` if it fails.
fn try_or_compiler_error(_: &Shared, state: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let (Some(message), Some(f)) = (a.positional.first(), a.positional.get(1)) else {
        return Err(jinja_error(
            "try_or_compiler_error() takes a message, a function and its arguments",
        ));
    };
    f.call(state, &a.positional[2..], &[])
        .map_err(|_| jinja_error(message.to_string()))
}

fn fromyaml(_: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let text = a.string(0, "value")?;
    let options = serde_saphyr::options! { strict_booleans: true };
    serde_saphyr::from_str_with_options::<Json>(&text, options)
        .map(|v| Value::from_serialize(&v))
        .map_err(|e| jinja_error(format!("fromyaml(): {e}")))
}

fn toyaml(_: &Shared, _: &State<'_, '_>, a: &Args<'_>) -> Result<Value, Error> {
    let v = to_json(&a.get(0, "value").unwrap_or_default())?;
    serde_saphyr::to_string(&v)
        .map(Value::from)
        .map_err(|e| jinja_error(format!("toyaml(): {e}")))
}

/// `run_started_at`: now, in UTC, as a Python datetime.
pub fn run_started_at() -> Value {
    Value::from_object(PyDateTime::new_aware(
        chrono::Utc::now().with_timezone(&Tz::UTC),
        Some(PytzTimezone { tz: Tz::UTC }),
    ))
}

/// `invocation_id`: a random UUID (version 4) for this invocation.
pub fn invocation_id() -> Value {
    let half = || {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        );
        h.finish()
    };
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&half().to_be_bytes());
    bytes[8..].copy_from_slice(&half().to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Value::from(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// `flags`: the invocation's command-line flags, as dbt exposes them.
#[derive(Debug)]
pub struct Flags(pub Arc<Shared>);

impl Object for Flags {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let flags = self.0.flags.lock().unwrap();
        Some(match key.as_str()? {
            "WHICH" => Value::from(flags.which),
            "FULL_REFRESH" => Value::from(flags.full_refresh),
            "STORE_FAILURES" | "WARN_ERROR" | "FAIL_FAST" | "DEBUG" | "EMPTY" => Value::from(false),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_ids_are_uuids() {
        let id = invocation_id().to_string();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_ne!(id, invocation_id().to_string());
    }
}
