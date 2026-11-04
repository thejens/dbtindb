//! Finding the files of a dbt project on disk.

use std::fs;
use std::path::Path;

use dbtindb_core::{PROJECT_FILE, Settings};

/// A project file: its path relative to the project root, with `/`
/// separators on every platform, and its contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub path: String,
    pub source: String,
}

/// What dbtindb reads under the project's paths; everything else (docs,
/// `target/`, editor files) stays out of the database.
const EXTENSIONS: &[&str] = &["sql", "yml", "yaml", "csv"];

/// Files dbt would use but dbtindb cannot, reported as warnings.
const UNSUPPORTED: &[(&str, &str)] = &[(
    "selectors.yml",
    "selectors.yml is not supported; pass selectors to the commands",
)];

/// Where a project lists the packages `dbt deps` installs.
const PACKAGE_LISTS: &[&str] = &["packages.yml", "dependencies.yml"];

/// The project's files, with those of the packages `dbt deps` installed,
/// sorted by path, and warnings about files it skips.
pub fn collect(root: &Path) -> Result<(Vec<File>, Vec<String>), String> {
    let mut files = Vec::new();
    let settings = add_project(root, "", &mut files)?;

    let mut warnings: Vec<String> = UNSUPPORTED
        .iter()
        .filter(|(name, _)| root.join(name).exists())
        .map(|(name, why)| format!("{name}: {why}"))
        .collect();
    let install = root.join(&settings.packages_install_path);
    if install.is_dir() {
        let mut packages = Vec::new();
        for entry in fs::read_dir(&install).map_err(|e| format!("{}: {e}", install.display()))? {
            let entry = entry.map_err(|e| format!("{}: {e}", install.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') && entry.path().join(PROJECT_FILE).is_file() {
                packages.push(name);
            }
        }
        packages.sort();
        for name in packages {
            let rel = format!("{}/{name}", settings.packages_install_path);
            add_project(&root.join(&rel), &rel, &mut files)?;
        }
    } else if let Some(list) = PACKAGE_LISTS.iter().find(|l| root.join(l).exists()) {
        warnings.push(format!(
            "{list}: no packages installed in {}/; run `dbt deps` first",
            settings.packages_install_path
        ));
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    // Overlapping paths (`models` and `models/macros`) would list a file twice.
    files.dedup_by(|a, b| a.path == b.path);
    Ok((files, warnings))
}

/// Adds the project at `dir` (the root, or an installed package at `rel`):
/// its `dbt_project.yml` and the files under its paths.
fn add_project(dir: &Path, rel: &str, files: &mut Vec<File>) -> Result<Settings, String> {
    let project_file = dir.join(PROJECT_FILE);
    let text = fs::read_to_string(&project_file).map_err(|e| {
        format!(
            "{}: {e} (is {} a dbt project?)",
            project_file.display(),
            dir.display()
        )
    })?;
    let settings =
        Settings::parse(&text).map_err(|e| format!("{}: {e}", project_file.display()))?;
    let prefix = |p: &str| {
        if rel.is_empty() {
            p.to_owned()
        } else {
            format!("{rel}/{p}")
        }
    };
    files.push(File {
        path: prefix(PROJECT_FILE),
        source: text,
    });
    let mut dirs: Vec<&String> = [
        &settings.model_paths,
        &settings.seed_paths,
        &settings.snapshot_paths,
        &settings.test_paths,
        &settings.macro_paths,
    ]
    .into_iter()
    .flatten()
    .collect();
    dirs.sort();
    dirs.dedup();
    for path in dirs {
        // dbt's default paths need not all exist.
        if dir.join(path).is_dir() {
            walk(dir, path, &prefix(path), files)?;
        }
    }
    Ok(settings)
}

/// Adds the files under `root/rel` to `files`, as `stored/...`, skipping
/// hidden entries.
fn walk(root: &Path, rel: &str, stored: &str, files: &mut Vec<File>) -> Result<(), String> {
    let dir = root.join(rel);
    let entries = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(format!(
                "{}: file name is not valid UTF-8",
                entry.path().display()
            ));
        };
        if name.starts_with('.') {
            continue;
        }
        let path = format!("{stored}/{name}");
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", entry.path().display()))?;
        if kind.is_dir() {
            walk(root, &format!("{rel}/{name}"), &path, files)?;
        } else if EXTENSIONS
            .iter()
            .any(|ext| name.ends_with(&format!(".{ext}")))
        {
            let source = fs::read_to_string(entry.path())
                .map_err(|e| format!("{path}: {e} (dbtindb stores files as UTF-8 text)"))?;
            if source.contains('\0') {
                return Err(format!(
                    "{path}: contains a NUL byte, which a SQL text value cannot hold"
                ));
            }
            files.push(File { path, source });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory, removed when dropped.
    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new(name: &str, files: &[(&str, &str)]) -> Self {
            let root =
                std::env::temp_dir().join(format!("dbtindb-cli-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            for (path, text) in files {
                let p = root.join(path);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(p, text).unwrap();
            }
            Dir(root)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn collects_what_the_project_paths_hold() {
        let dir = Dir::new(
            "collect",
            &[
                ("dbt_project.yml", "name: shop\nmacro-paths: [lib]\n"),
                ("models/staging/a.sql", "select 1"),
                ("models/schema.yml", "models: []"),
                ("models/docs.md", "{% docs x %}{% enddocs %}"),
                ("models/.a.sql.swp", "junk"),
                ("seeds/c.csv", "x\n1\n"),
                ("lib/m.sql", "{% macro m() %}{% endmacro %}"),
                ("macros/ignored.sql", "not a macro path"),
                ("target/compiled.sql", "build output"),
                ("packages.yml", "packages: []"),
            ],
        );
        let (files, warnings) = collect(&dir.0).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "dbt_project.yml",
                "lib/m.sql",
                "models/schema.yml",
                "models/staging/a.sql",
                "seeds/c.csv"
            ]
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("run `dbt deps` first"), "{warnings:?}");
    }

    #[test]
    fn collects_installed_packages() {
        let dir = Dir::new(
            "packages",
            &[
                (
                    "dbt_project.yml",
                    "name: shop
",
                ),
                ("packages.yml", "packages: []"),
                ("models/a.sql", "select 1"),
                (
                    "dbt_packages/dbt_utils/dbt_project.yml",
                    "name: dbt_utils
",
                ),
                (
                    "dbt_packages/dbt_utils/macros/sql/star.sql",
                    "{% macro star() %}{% endmacro %}",
                ),
                (
                    "dbt_packages/dbt_utils/integration_tests/models/m.sql",
                    "select 1",
                ),
                ("dbt_packages/dbt_utils/README.md", "docs"),
                ("dbt_packages/stray/notes.sql", "not a package"),
            ],
        );
        let (files, warnings) = collect(&dir.0).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "dbt_packages/dbt_utils/dbt_project.yml",
                "dbt_packages/dbt_utils/macros/sql/star.sql",
                "dbt_project.yml",
                "models/a.sql",
            ]
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn needs_a_project_file() {
        let dir = Dir::new("missing", &[("models/a.sql", "select 1")]);
        let err = collect(&dir.0).unwrap_err();
        assert!(
            err.contains("is") && err.contains("a dbt project?"),
            "{err}"
        );
    }
}
