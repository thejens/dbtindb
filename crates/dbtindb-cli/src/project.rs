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
const UNSUPPORTED: &[(&str, &str)] = &[
    (
        "packages.yml",
        "packages are not supported; copy the macros you need into macros/",
    ),
    (
        "dependencies.yml",
        "packages are not supported; copy the macros you need into macros/",
    ),
    (
        "selectors.yml",
        "selectors.yml is not supported; pass selectors to the commands",
    ),
];

/// The project's files, sorted by path, and warnings about files it skips.
pub fn collect(root: &Path) -> Result<(Vec<File>, Vec<String>), String> {
    let project_file = root.join(PROJECT_FILE);
    let text = fs::read_to_string(&project_file).map_err(|e| {
        format!(
            "{}: {e} (is {} a dbt project?)",
            project_file.display(),
            root.display()
        )
    })?;
    let settings = Settings::parse(&text).map_err(|e| e.to_string())?;

    let mut files = vec![File {
        path: PROJECT_FILE.to_owned(),
        source: text,
    }];
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
    for dir in dirs {
        // dbt's default paths need not all exist.
        if root.join(dir).is_dir() {
            walk(root, dir, &mut files)?;
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    // Overlapping paths (`models` and `models/macros`) would list a file twice.
    files.dedup_by(|a, b| a.path == b.path);

    let warnings = UNSUPPORTED
        .iter()
        .filter(|(name, _)| root.join(name).exists())
        .map(|(name, why)| format!("{name}: {why}"))
        .collect();
    Ok((files, warnings))
}

/// Adds the files under `root/rel` to `files`, skipping hidden entries.
fn walk(root: &Path, rel: &str, files: &mut Vec<File>) -> Result<(), String> {
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
        let path = format!("{rel}/{name}");
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", entry.path().display()))?;
        if kind.is_dir() {
            walk(root, &path, files)?;
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
        assert!(warnings[0].starts_with("packages.yml"), "{warnings:?}");
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
