//! Installed packages: the directories `dbt deps` writes under
//! `packages-install-path` (`dbt_packages/` by default).
//!
//! A package is a dbt project of its own. Its macros are called through its
//! name (`dbt_utils.star(...)`), and its models, seeds, snapshots, tests
//! and analyses become nodes of the run, namespaced by the package as in
//! dbt: `model.<package>.<name>`, `ref('<package>', '<name>')`.

use std::collections::{BTreeMap, BTreeSet};

use super::Resources;
use super::settings::{PROJECT_FILE, Settings};
use crate::Error;

/// One installed package.
#[derive(Clone, Debug)]
pub struct Package {
    /// The name from the package's own `dbt_project.yml`: its namespace.
    pub name: String,
    /// The directory it is installed in, e.g. `dbt_packages/dbt_utils`.
    pub dir: String,
    /// Its own `dbt_project.yml`, with resource paths relative to `dir`.
    /// Its config trees apply to its nodes, under the root project's.
    pub settings: Settings,
    /// Its nodes, macros and properties.
    pub resources: Resources,
}

/// Finds the packages among `files`, given the root project's settings.
/// Returns them by name, without their resources, and the paths of every
/// file they own.
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
        let dir = format!("{install}/{dir}");
        owned.extend(paths.iter().map(|p| p.to_string()));
        let project_file = format!("{dir}/{PROJECT_FILE}");
        let Some(text) = files.get(&project_file) else {
            // dbt deps always installs a dbt_project.yml; anything else is
            // not a package (a stray README, a lock file).
            continue;
        };
        let own = Settings::parse(text).map_err(|e| Error::new(format!("{project_file}: {e}")))?;
        if own.name == settings.name {
            return Err(Error::new(format!(
                "package {dir} has the root project's name {:?}",
                own.name
            )));
        }
        if let Some(other) = packages.get(&own.name) {
            return Err(Error::new(format!(
                "two packages are named {:?}: {} and {dir}",
                own.name, other.dir
            )));
        }
        packages.insert(
            own.name.clone(),
            Package {
                name: own.name.clone(),
                settings: own,
                dir,
                resources: Resources::default(),
            },
        );
    }
    Ok((packages, owned))
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
    fn finds_packages_by_their_project_file() {
        let (pkgs, owned) = packages(
            &Settings::default(),
            &files(&[
                (
                    "dbt_packages/dbt_utils/dbt_project.yml",
                    "name: dbt_utils\nvars:\n  x: 1\n",
                ),
                ("dbt_packages/dbt_utils/macros/sql/star.sql", ""),
                ("dbt_packages/notes/README.md", ""),
                ("models/a.sql", ""),
            ]),
        )
        .unwrap();
        assert_eq!(pkgs.keys().collect::<Vec<_>>(), ["dbt_utils"]);
        let utils = &pkgs["dbt_utils"];
        assert_eq!(utils.dir, "dbt_packages/dbt_utils");
        assert_eq!(utils.settings.vars["x"], 1);
        assert_eq!(owned.len(), 3);
    }

    #[test]
    fn rejects_duplicate_names() {
        let err = packages(
            &Settings::default(),
            &files(&[
                ("dbt_packages/a/dbt_project.yml", "name: p\n"),
                ("dbt_packages/b/dbt_project.yml", "name: p\n"),
            ]),
        )
        .unwrap_err();
        assert!(err.0.contains("two packages are named \"p\""), "{err}");
    }
}
