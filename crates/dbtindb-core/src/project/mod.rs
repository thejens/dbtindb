//! Turning the rows of `dbtindb_files` into a dbt project.
//!
//! Each row is a file, named by its path relative to the project root, as
//! in a dbt project on disk: `dbt_project.yml`, `models/**/*.sql`,
//! `models/**/*.yml`, `seeds/**/*.csv`, `snapshots/**/*.sql`,
//! `tests/**/*.sql`, `macros/**/*.sql`. Paths can be changed in
//! `dbt_project.yml` as in dbt. Any other file can still be included or
//! imported by name. Installed packages live under `dbt_packages/`.

mod docs;
mod packages;
mod properties;
mod settings;
pub mod unit_tests;

use std::collections::BTreeMap;

pub use packages::Package;
pub use properties::{ColumnProps, Properties, ResourceProps, SourceTable, TestDef};
pub use settings::{PROJECT_FILE, Settings};
use unit_tests::FixtureFiles;

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
    /// The project or package that defines it.
    pub package: String,
}

/// What one project, the root or an installed package, defines.
#[derive(Clone, Debug, Default)]
pub struct Resources {
    pub models: BTreeMap<String, NodeFile>,
    pub seeds: BTreeMap<String, NodeFile>,
    pub singular_tests: BTreeMap<String, NodeFile>,
    /// Analyses: SQL that is compiled but never run.
    pub analyses: BTreeMap<String, NodeFile>,
    /// Files that hold `{% snapshot %}` blocks. Snapshot names come from
    /// the blocks, so they are only known once the files are rendered.
    pub snapshot_files: Vec<NodeFile>,
    /// Files whose macros (and `{% test %}` blocks) it defines, sorted.
    pub macro_files: Vec<String>,
    pub properties: Properties,
    /// The text of each `{% docs %}` block, by name.
    pub docs: BTreeMap<String, String>,
}

/// A whole project: every file, classified.
#[derive(Clone, Debug)]
pub struct Project {
    pub settings: Settings,
    /// All files by path.
    pub files: BTreeMap<String, String>,
    /// What the root project defines.
    pub root: Resources,
    /// Installed packages, by name.
    pub packages: BTreeMap<String, Package>,
}

impl Project {
    /// Builds a project from `(path, contents)` pairs.
    pub fn new(files: impl IntoIterator<Item = (String, String)>) -> Result<Self, Error> {
        let files: BTreeMap<String, String> = files.into_iter().collect();
        let mut settings = match files.get(PROJECT_FILE) {
            Some(text) => Settings::parse(text)?,
            None => Settings::default(),
        };
        let (mut packages, package_paths) = packages::packages(&settings, &files)?;
        scope_vars(&mut settings, &packages);
        let root = classify_all(
            &settings,
            files.iter().filter(|(p, _)| !package_paths.contains(*p)),
        )?;
        for package in packages.values_mut() {
            let prefix = format!("{}/", package.dir);
            package.resources = classify_all(
                &package.settings.rooted(&package.dir),
                files.iter().filter(|(p, _)| p.starts_with(&prefix)),
            )?;
        }
        Ok(Project {
            settings,
            files,
            root,
            packages,
        })
    }

    /// The root project and every package, root first: each one's name,
    /// own settings and resources.
    pub fn all(&self) -> impl Iterator<Item = (&str, &Settings, &Resources)> {
        std::iter::once((self.settings.name.as_str(), &self.settings, &self.root)).chain(
            self.packages
                .values()
                .map(|p| (p.name.as_str(), &p.settings, &p.resources)),
        )
    }

    /// What the project or package called `package` defines.
    pub fn resources(&self, package: &str) -> Option<&Resources> {
        if package == self.settings.name {
            return Some(&self.root);
        }
        self.packages.get(package).map(|p| &p.resources)
    }

    /// The settings of the project or package called `package`.
    pub fn settings_of(&self, package: &str) -> Option<&Settings> {
        if package == self.settings.name {
            return Some(&self.settings);
        }
        self.packages.get(package).map(|p| &p.settings)
    }

    /// The package a file belongs to, if it is in one.
    pub fn package_of(&self, path: &str) -> Option<&Package> {
        self.packages.values().find(|p| {
            path.strip_prefix(p.dir.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        })
    }

    /// Every source table, from the root project's property files and
    /// the packages'. Sources are looked up by name alone, so a name
    /// declared twice is an error.
    pub fn sources(&self) -> Result<BTreeMap<(String, String), SourceTable>, Error> {
        let mut all: BTreeMap<(String, String), SourceTable> = BTreeMap::new();
        for (_, _, r) in self.all() {
            for (key, table) in &r.properties.sources {
                if let Some(prev) = all.insert(key.clone(), table.clone()) {
                    return Err(Error::new(format!(
                        "source {:?} table {:?} is declared in both {} and {}",
                        key.0, key.1, prev.file, table.file
                    )));
                }
            }
        }
        Ok(all)
    }
}

/// Classifies the files of one project, given its settings with resource
/// paths relative to the project root.
fn classify_all<'a>(
    settings: &Settings,
    files: impl Iterator<Item = (&'a String, &'a String)> + Clone,
) -> Result<Resources, Error> {
    // Fixture files first: property files read them as they are added.
    let mut fixtures = FixtureFiles::new();
    for (path, text) in files.clone() {
        if let Some((name, ext)) = fixture(settings, path)
            && fixtures
                .insert((name.clone(), ext.clone()), text.clone())
                .is_some()
        {
            return Err(Error::new(format!(
                "{path}: there are two fixture files named {name}.{ext}"
            )));
        }
    }
    let mut r = Resources::default();
    for (path, text) in files {
        classify(&mut r, settings, &fixtures, path, text)?;
    }
    Ok(r)
}

/// A file under `fixtures/` in a test path: a unit test fixture, by name
/// and extension.
fn fixture(s: &Settings, path: &str) -> Option<(String, String)> {
    let file = under(&s.test_paths, path, &s.name)?;
    if file.dirs.first().map(String::as_str) != Some("fixtures") {
        return None;
    }
    let ext = path.rsplit_once('.').map(|(_, e)| e)?;
    matches!(ext, "csv" | "sql").then(|| (file.name, ext.to_owned()))
}

fn classify(
    r: &mut Resources,
    s: &Settings,
    fixtures: &FixtureFiles,
    path: &str,
    text: &str,
) -> Result<(), Error> {
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let package = &s.name;
    let yaml = matches!(ext, "yml" | "yaml") && !path.ends_with(PROJECT_FILE);
    let roots = [
        &s.model_paths,
        &s.seed_paths,
        &s.snapshot_paths,
        &s.test_paths,
        &s.macro_paths,
        &s.analysis_paths,
    ];
    if yaml && roots.iter().any(|r| under(r, path, package).is_some()) {
        return r.properties.add_file(path, text, fixtures);
    }
    if fixture(s, path).is_some() {
        return Ok(());
    }
    if ext == "sql" {
        if let Some(file) = under(&s.model_paths, path, package) {
            return insert(&mut r.models, "model", file);
        }
        if let Some(file) = under(&s.snapshot_paths, path, package) {
            r.snapshot_files.push(file);
            return Ok(());
        }
        if let Some(file) = under(&s.test_paths, path, package) {
            // tests/generic/ holds custom generic tests, which are
            // defined like macros, with {% test %} blocks.
            if file.dirs.first().map(String::as_str) == Some("generic") {
                r.macro_files.push(path.to_owned());
                return Ok(());
            }
            return insert(&mut r.singular_tests, "test", file);
        }
        if let Some(file) = under(&s.analysis_paths, path, package) {
            return insert(&mut r.analyses, "analysis", file);
        }
        if under(&s.macro_paths, path, package).is_some() {
            r.macro_files.push(path.to_owned());
            return Ok(());
        }
    }
    if ext == "md" && under(&s.docs_roots(), path, package).is_some() {
        return docs::add_file(&mut r.docs, path, text);
    }
    if ext == "csv"
        && let Some(file) = under(&s.seed_paths, path, package)
    {
        return insert(&mut r.seeds, "seed", file);
    }
    Ok(())
}

/// Vars scoped to a package (`vars: {dbt_utils: {...}}`) are global, as
/// dbtindb has one var namespace, and a package's own vars are defaults
/// the project can override.
fn scope_vars(settings: &mut Settings, packages: &BTreeMap<String, Package>) {
    for (name, package) in packages {
        if let Some(serde_json::Value::Object(scoped)) = settings.vars.get(name).cloned() {
            settings.vars.remove(name);
            settings.vars.extend(scoped);
        }
        for (k, v) in &package.settings.vars {
            if !settings.vars.contains_key(k) {
                settings.vars.insert(k.clone(), v.clone());
            }
        }
    }
}

impl Project {
    /// The node a property file declares by name, such as a YAML snapshot
    /// or a unit test, placed in the folder tree of the property file.
    pub fn declared_in(&self, path: &str, name: &str) -> NodeFile {
        let (s, package) = match self.package_of(path) {
            Some(p) => (p.settings.rooted(&p.dir), p.name.clone()),
            None => (self.settings.clone(), self.settings.name.clone()),
        };
        let roots: Vec<String> = [
            &s.model_paths,
            &s.seed_paths,
            &s.snapshot_paths,
            &s.test_paths,
            &s.analysis_paths,
            &s.macro_paths,
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect();
        let dirs = under(&roots, path, &package)
            .map(|f| f.dirs)
            .unwrap_or_default();
        NodeFile {
            name: name.to_owned(),
            path: path.to_owned(),
            dirs,
            package,
        }
    }
}

/// The node file at `path`, of `package`, if it lies under one of `roots`.
fn under(roots: &[String], path: &str, package: &str) -> Option<NodeFile> {
    roots.iter().find_map(|root| {
        let rest = path.strip_prefix(root.as_str())?.strip_prefix('/')?;
        let mut parts: Vec<&str> = rest.split('/').collect();
        let file = parts.pop()?;
        let name = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
        (!name.is_empty()).then(|| NodeFile {
            name: name.to_owned(),
            path: path.to_owned(),
            dirs: parts.into_iter().map(str::to_owned).collect(),
            package: package.to_owned(),
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
            p.root.models.keys().collect::<Vec<_>>(),
            ["orders", "stg_orders"]
        );
        assert_eq!(p.root.models["stg_orders"].dirs, ["stg"]);
        assert_eq!(p.root.seeds.keys().collect::<Vec<_>>(), ["countries"]);
        assert_eq!(
            p.root.singular_tests.keys().collect::<Vec<_>>(),
            ["assert_positive"]
        );
        assert_eq!(
            p.root.snapshot_files[0].path,
            "snapshots/orders_snapshot.sql"
        );
        assert_eq!(
            p.root.macro_files,
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
        assert_eq!(p.root.models.keys().collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn classifies_package_files_and_fixtures() {
        let p = Project::new(
            [
                ("dbt_project.yml", "name: shop\n"),
                ("tests/fixtures/orders.csv", "id\n1\n"),
                ("tests/fixtures/deep/more.sql", "select 1"),
                ("tests/check.sql", ""),
                (
                    "dbt_packages/audit/dbt_project.yml",
                    "name: audit\nmodel-paths: [src]\n",
                ),
                ("dbt_packages/audit/src/log/events.sql", ""),
                ("dbt_packages/audit/seeds/kinds.csv", ""),
                ("dbt_packages/audit/macros/m.sql", ""),
                ("dbt_packages/audit/models/ignored.sql", ""),
                ("dbt_packages/audit/integration_tests/models/x.sql", ""),
            ]
            .map(|(p, t)| (p.to_owned(), t.to_owned())),
        )
        .unwrap();
        assert_eq!(p.root.singular_tests.keys().collect::<Vec<_>>(), ["check"]);
        let audit = &p.packages["audit"].resources;
        assert_eq!(audit.models.keys().collect::<Vec<_>>(), ["events"]);
        assert_eq!(audit.models["events"].dirs, ["log"]);
        assert_eq!(audit.models["events"].package, "audit");
        assert_eq!(audit.seeds.keys().collect::<Vec<_>>(), ["kinds"]);
        assert_eq!(audit.macro_files, ["dbt_packages/audit/macros/m.sql"]);
        assert!(p.root.models.is_empty());
        assert_eq!(
            p.package_of("dbt_packages/audit/src/log/events.sql")
                .map(|p| p.name.as_str()),
            Some("audit")
        );
    }

    #[test]
    fn rejects_duplicate_model_names() {
        let err = project(&["models/a/x.sql", "models/b/x.sql"]).unwrap_err();
        assert!(err.0.contains("two models are named \"x\""), "{err}");
    }
}
