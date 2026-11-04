//! Turning a set of template files into a dbt-style project.

use std::collections::BTreeMap;

use crate::Error;

/// One model: a file under `models/`.
#[derive(Clone, Debug)]
pub struct Model {
    /// The name `ref()` uses and the relation is created as: the file name
    /// without its directory and `.sql`, as in dbt.
    pub name: String,
    /// The template path, e.g. `models/staging/stg_orders.sql`.
    pub path: String,
}

/// A project: every template file, with the models identified.
#[derive(Clone, Debug)]
pub struct Project {
    /// All files by path. Models, macros and anything else they include.
    pub files: BTreeMap<String, String>,
    /// Models by name.
    pub models: BTreeMap<String, Model>,
    /// Paths of the files whose macros are global, sorted.
    pub macro_files: Vec<String>,
}

impl Project {
    /// Builds a project from `(path, source)` pairs.
    pub fn new(files: impl IntoIterator<Item = (String, String)>) -> Result<Self, Error> {
        let files: BTreeMap<String, String> = files.into_iter().collect();
        let mut models: BTreeMap<String, Model> = BTreeMap::new();
        let mut macro_files = Vec::new();
        for path in files.keys() {
            if let Some(rest) = path
                .strip_prefix("models/")
                .filter(|_| path.ends_with(".sql"))
            {
                let file = rest.rsplit('/').next().unwrap_or(rest);
                let name = file.strip_suffix(".sql").unwrap_or(file).to_owned();
                if name.is_empty() {
                    return Err(Error::new(format!("model file {path:?} has an empty name")));
                }
                if let Some(existing) = models.get(&name) {
                    // dbt rejects this too: ref() must be unambiguous.
                    return Err(Error::new(format!(
                        "two models are named {name:?}: {} and {path}",
                        existing.path
                    )));
                }
                models.insert(
                    name.clone(),
                    Model {
                        name,
                        path: path.clone(),
                    },
                );
            } else if path.starts_with("macros/") && path.ends_with(".sql") {
                macro_files.push(path.clone());
            }
        }
        Ok(Self {
            files,
            models,
            macro_files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> Vec<(String, String)> {
        paths
            .iter()
            .map(|p| (p.to_string(), String::new()))
            .collect()
    }

    #[test]
    fn classifies_files() {
        let p = Project::new(files(&[
            "models/stg/stg_orders.sql",
            "models/orders.sql",
            "macros/money.sql",
            "partials/header.sql",
            "models/readme.md",
        ]))
        .unwrap();
        assert_eq!(
            p.models.keys().collect::<Vec<_>>(),
            ["orders", "stg_orders"]
        );
        assert_eq!(p.models["stg_orders"].path, "models/stg/stg_orders.sql");
        assert_eq!(p.macro_files, ["macros/money.sql"]);
    }

    #[test]
    fn rejects_duplicate_model_names() {
        let err = Project::new(files(&["models/a/x.sql", "models/b/x.sql"])).unwrap_err();
        assert!(err.0.contains("two models are named \"x\""), "{err}");
    }
}
