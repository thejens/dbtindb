//! Installed packages: the directories `dbt deps` writes under
//! `packages-install-path` (`dbt_packages/` by default).
//!
//! Packages contribute macros and generic tests, which is what the common
//! utility packages (dbt_utils, dbt_date, dbt_expectations, codegen) are.
//! A package's models, seeds and snapshots would need their own namespace
//! in `ref()` and in the managed relations, so they are rejected rather
//! than silently dropped.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::settings::{PROJECT_FILE, Settings};
use crate::Error;

/// One installed package.
#[derive(Clone, Debug)]
pub struct Package {
    /// The name from the package's own `dbt_project.yml`: the namespace
    /// its macros are called through (`dbt_utils.star(...)`).
    pub name: String,
    /// Files that define its macros and generic tests, sorted.
    pub macro_files: Vec<String>,
    /// The package's own `vars:`, defaults for the project's.
    pub vars: Map<String, Value>,
}

/// Finds the packages among `files`, given the root project's settings.
/// Returns them by name, with the paths of every file they own.
pub fn packages(
    settings: &Settings,
    files: &BTreeMap<String, String>,
) -> Result<(BTreeMap<String, Package>, BTreeSet<String>), Error> {
    let install = settings.packages_install_path.as_str();
    let mut by_dir: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for path in files.keys() {
        let Some(rest) = path.strip_prefix(install).and_then(|r| r.strip_prefix('/')) else {
            continue;
        };
        if let Some((dir, _)) = rest.split_once('/') {
            by_dir.entry(dir).or_default().push(path);
        }
    }

    let mut packages: BTreeMap<String, Package> = BTreeMap::new();
    let mut owned = BTreeSet::new();
    for (dir, paths) in by_dir {
        let root = format!("{install}/{dir}");
        owned.extend(paths.iter().map(|p| p.to_string()));
        let project_file = format!("{root}/{PROJECT_FILE}");
        let Some(text) = files.get(&project_file) else {
            // dbt deps always installs a dbt_project.yml; anything else is
            // not a package (a stray README, a lock file).
            continue;
        };
        let own = Settings::parse(text).map_err(|e| Error::new(format!("{project_file}: {e}")))?;
        let mut macro_files = Vec::new();
        for path in paths {
            let Some(rel) = path.strip_prefix(&root).and_then(|r| r.strip_prefix('/')) else {
                continue;
            };
            if !rel.ends_with(".sql") {
                continue;
            }
            let generic_test = own.test_paths.iter().any(|t| {
                rel.strip_prefix(t.as_str())
                    .is_some_and(|r| r.starts_with("/generic/"))
            });
            if generic_test || within(&own.macro_paths, rel) {
                macro_files.push(path.to_owned());
                continue;
            }
            for (kind, roots) in [
                ("models", &own.model_paths),
                ("seeds", &own.seed_paths),
                ("snapshots", &own.snapshot_paths),
            ] {
                if within(roots, rel) {
                    return Err(Error::new(format!(
                        "package {} ({root}) defines {kind} ({path}); dbtindb supports \
                         the macros and generic tests of packages, not their {kind}",
                        own.name
                    )));
                }
            }
        }
        if let Some(other) = packages.get(&own.name) {
            return Err(Error::new(format!(
                "two packages are named {:?}: {root} and the one with {}",
                own.name,
                other
                    .macro_files
                    .first()
                    .map_or("no macros", String::as_str)
            )));
        }
        packages.insert(
            own.name.clone(),
            Package {
                name: own.name,
                macro_files,
                vars: own.vars,
            },
        );
    }
    Ok((packages, owned))
}

fn within(roots: &[String], rel: &str) -> bool {
    roots.iter().any(|r| {
        rel.strip_prefix(r.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .collect()
    }

    #[test]
    fn finds_macros_and_generic_tests() {
        let (pkgs, owned) = packages(
            &Settings::default(),
            &files(&[
                (
                    "dbt_packages/dbt_utils/dbt_project.yml",
                    "name: dbt_utils\nvars:\n  x: 1\n",
                ),
                ("dbt_packages/dbt_utils/macros/sql/star.sql", ""),
                (
                    "dbt_packages/dbt_utils/macros/generic_tests/at_least_one.sql",
                    "",
                ),
                ("dbt_packages/dbt_utils/tests/generic/t.sql", ""),
                ("dbt_packages/dbt_utils/integration_tests/models/m.sql", ""),
                ("dbt_packages/dbt_utils/README.md", ""),
                ("models/a.sql", ""),
            ]),
        )
        .unwrap();
        let utils = &pkgs["dbt_utils"];
        assert_eq!(
            utils.macro_files,
            [
                "dbt_packages/dbt_utils/macros/generic_tests/at_least_one.sql",
                "dbt_packages/dbt_utils/macros/sql/star.sql",
                "dbt_packages/dbt_utils/tests/generic/t.sql",
            ]
        );
        assert_eq!(utils.vars["x"], 1);
        assert_eq!(owned.len(), 6);
    }

    #[test]
    fn rejects_package_models() {
        let err = packages(
            &Settings::default(),
            &files(&[
                ("dbt_packages/p/dbt_project.yml", "name: p\n"),
                ("dbt_packages/p/models/m.sql", ""),
            ]),
        )
        .unwrap_err();
        assert!(err.0.contains("not their models"), "{err}");
    }
}
