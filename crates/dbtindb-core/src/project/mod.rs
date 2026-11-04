//! Turning the rows of `dbtindb_files` into a dbt project.
//!
//! Each row is a file, named by its path relative to the project root, as
//! in a dbt project on disk: `dbt_project.yml`, `models/**/*.sql`,
//! `models/**/*.yml`, `seeds/**/*.csv`, `snapshots/**/*.sql`,
//! `tests/**/*.sql`, `macros/**/*.sql`. Paths can be changed in
//! `dbt_project.yml` as in dbt. Any other file can still be included or
//! imported by name.

mod properties;
mod settings;

use std::collections::BTreeMap;

pub use properties::{ColumnProps, Properties, ResourceProps, SourceTable, TestDef};
pub use settings::{PROJECT_FILE, Settings};

use crate::Error;

/// A file that defines one node: a model, seed or singular test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeFile {
    /// The name `ref()` uses: the file name without directory or extension.
    pub name: String,
    /// The template path, e.g. `models/staging/stg_orders.sql`.
    pub path: String,
    /// The directories between the resource path and the file, for
    /// `dbt_project.yml` config and `path:` selectors.
    pub dirs: Vec<String>,
}

/// A whole project: every file, classified.
#[derive(Clone, Debug)]
pub struct Project {
    pub settings: Settings,
    /// All files by path.
    pub files: BTreeMap<String, String>,
    pub models: BTreeMap<String, NodeFile>,
    pub seeds: BTreeMap<String, NodeFile>,
    pub singular_tests: BTreeMap<String, NodeFile>,
    /// Files that hold `{% snapshot %}` blocks. Snapshot names come from
    /// the blocks, so they are only known once the files are rendered.
    pub snapshot_files: Vec<NodeFile>,
    /// Files whose macros (and `{% test %}` blocks) are global, sorted.
    pub macro_files: Vec<String>,
    pub properties: Properties,
}

impl Project {
    /// Builds a project from `(path, contents)` pairs.
    pub fn new(files: impl IntoIterator<Item = (String, String)>) -> Result<Self, Error> {
        let files: BTreeMap<String, String> = files.into_iter().collect();
        let settings = match files.get(PROJECT_FILE) {
            Some(text) => Settings::parse(text)?,
            None => Settings::default(),
        };
        let mut p = Project {
            settings,
            files: BTreeMap::new(),
            models: BTreeMap::new(),
            seeds: BTreeMap::new(),
            singular_tests: BTreeMap::new(),
            snapshot_files: Vec::new(),
            macro_files: Vec::new(),
            properties: Properties::default(),
        };
        for (path, text) in &files {
            p.classify(path, text)?;
        }
        p.files = files;
        Ok(p)
    }

    fn classify(&mut self, path: &str, text: &str) -> Result<(), Error> {
        let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        let s = &self.settings;
        let yaml = matches!(ext, "yml" | "yaml") && path != PROJECT_FILE;
        let roots = [
            &s.model_paths,
            &s.seed_paths,
            &s.snapshot_paths,
            &s.test_paths,
            &s.macro_paths,
        ];
        if yaml && roots.iter().any(|r| under(r, path).is_some()) {
            return self.properties.add_file(path, text);
        }
        if ext == "sql" {
            if let Some(file) = under(&s.model_paths, path) {
                return insert(&mut self.models, "model", file);
            }
            if let Some(file) = under(&s.snapshot_paths, path) {
                self.snapshot_files.push(file);
                return Ok(());
            }
            if let Some(file) = under(&s.test_paths, path) {
                // tests/generic/ holds custom generic tests, which are
                // defined like macros, with {% test %} blocks.
                if file.dirs.first().map(String::as_str) == Some("generic") {
                    self.macro_files.push(path.to_owned());
                    return Ok(());
                }
                return insert(&mut self.singular_tests, "test", file);
            }
            if under(&s.macro_paths, path).is_some() {
                self.macro_files.push(path.to_owned());
                return Ok(());
            }
        }
        if ext == "csv"
            && let Some(file) = under(&s.seed_paths, path)
        {
            return insert(&mut self.seeds, "seed", file);
        }
        Ok(())
    }
}

/// The node file at `path`, if it lies under one of `roots`.
fn under(roots: &[String], path: &str) -> Option<NodeFile> {
    roots.iter().find_map(|root| {
        let rest = path.strip_prefix(root.as_str())?.strip_prefix('/')?;
        let mut parts: Vec<&str> = rest.split('/').collect();
        let file = parts.pop()?;
        let name = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
        (!name.is_empty()).then(|| NodeFile {
            name: name.to_owned(),
            path: path.to_owned(),
            dirs: parts.into_iter().map(str::to_owned).collect(),
        })
    })
}

fn insert(map: &mut BTreeMap<String, NodeFile>, kind: &str, file: NodeFile) -> Result<(), Error> {
    if let Some(existing) = map.get(&file.name) {
        // dbt rejects this too: ref() must be unambiguous.
        return Err(Error::new(format!(
            "two {kind}s are named {:?}: {} and {}",
            file.name, existing.path, file.path
        )));
    }
    map.insert(file.name.clone(), file);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(paths: &[&str]) -> Result<Project, Error> {
        Project::new(paths.iter().map(|p| (p.to_string(), String::new())))
    }

    #[test]
    fn classifies_files() {
        let p = project(&[
            "models/stg/stg_orders.sql",
            "models/orders.sql",
            "models/schema.yml",
            "macros/money.sql",
            "seeds/countries.csv",
            "snapshots/orders_snapshot.sql",
            "tests/assert_positive.sql",
            "tests/generic/is_even.sql",
            "partials/header.sql",
            "models/readme.md",
        ])
        .unwrap();
        assert_eq!(
            p.models.keys().collect::<Vec<_>>(),
            ["orders", "stg_orders"]
        );
        assert_eq!(p.models["stg_orders"].dirs, ["stg"]);
        assert_eq!(p.seeds.keys().collect::<Vec<_>>(), ["countries"]);
        assert_eq!(
            p.singular_tests.keys().collect::<Vec<_>>(),
            ["assert_positive"]
        );
        assert_eq!(p.snapshot_files[0].path, "snapshots/orders_snapshot.sql");
        assert_eq!(
            p.macro_files,
            ["macros/money.sql", "tests/generic/is_even.sql"]
        );
    }

    #[test]
    fn honours_custom_paths() {
        let p = Project::new([
            (
                "dbt_project.yml".to_owned(),
                "name: x\nmodel-paths: [transform]\n".to_owned(),
            ),
            ("transform/a.sql".to_owned(), String::new()),
            ("models/b.sql".to_owned(), String::new()),
        ])
        .unwrap();
        assert_eq!(p.models.keys().collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn rejects_duplicate_model_names() {
        let err = project(&["models/a/x.sql", "models/b/x.sql"]).unwrap_err();
        assert!(err.0.contains("two models are named \"x\""), "{err}");
    }
}
