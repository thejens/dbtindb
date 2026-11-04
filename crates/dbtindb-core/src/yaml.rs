//! YAML files of a project: `dbt_project.yml` and property files.

use serde_json::Value;

use crate::Error;

/// Parses YAML into JSON values. An empty document is an empty mapping.
///
/// Booleans follow YAML 1.2 Core (`true`/`false` only). With YAML 1.1 rules a
/// column named `n` or an accepted value `NO` (Norway) would silently become
/// `false`, which corrupts names and test arguments.
pub fn parse(path: &str, text: &str) -> Result<Value, Error> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let options = serde_saphyr::options! { strict_booleans: true };
    let value: Value = serde_saphyr::from_str_with_options(text, options)
        .map_err(|e| Error::new(format!("{path}: {e}")))?;
    match value {
        Value::Null => Ok(Value::Object(Default::default())),
        v @ Value::Object(_) => Ok(v),
        _ => Err(Error::new(format!(
            "{path}: expected a mapping at the top level"
        ))),
    }
}
