//! Snapshots: their names come from `{% snapshot %}` blocks and from dbt
//! 1.9's YAML snapshots, and each becomes a macro.

use crate::Error;
use crate::context::in_package;
use crate::project::NodeFile;
use crate::session::Session;

/// Registers every snapshot, from `{% snapshot %}` blocks and from dbt
/// 1.9's YAML snapshots, as the macro `<package>.snapshot_<name>`.
/// Returns their names and files.
pub(super) fn snapshot_names(session: &mut Session) -> Result<Vec<(String, NodeFile)>, Error> {
    let root = session.shared.project_name.clone();
    let scoped = |package: &str, source: &str| {
        if package == root {
            source.to_owned()
        } else {
            in_package(package, source)
        }
    };
    let mut snapshots: Vec<(String, NodeFile)> = Vec::new();
    let files: Vec<NodeFile> = session
        .project
        .all()
        .flat_map(|(_, _, r)| r.snapshot_files.iter().cloned())
        .collect();
    for file in files {
        let source = scoped(&file.package, &session.project.files[&file.path]);
        for name in crate::context::macro_names(&mut session.env, &file.path, &source)? {
            if let Some(snapshot) = name.strip_prefix("snapshot_") {
                // Callable by name through dbt's global macro namespace.
                session.env.add_template_owned(
                    format!("{}.{name}", file.package),
                    source.clone(),
                    Some(file.path.clone()),
                )?;
                snapshots.push((snapshot.to_owned(), file.clone()));
            }
        }
    }
    // dbt 1.9's YAML snapshots name a `relation:` instead of a SELECT.
    let yaml_snapshots: Vec<(String, String, NodeFile)> = session
        .project
        .all()
        .flat_map(|(_, _, r)| r.properties.snapshots.iter())
        .filter_map(|(name, props)| {
            let relation = props.relation.clone()?;
            Some((
                name.clone(),
                relation,
                session.project.declared_in(&props.file, name),
            ))
        })
        .collect();
    for (name, relation, file) in yaml_snapshots {
        if let Some((_, sql)) = snapshots
            .iter()
            .find(|(n, f)| *n == name && f.package == file.package)
        {
            return Err(Error::new(format!(
                "{}: snapshot {name:?} is defined both here and in {}",
                file.path, sql.path
            )));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::new(format!(
                "{}: snapshot name {name:?} must be letters, digits and underscores",
                file.path
            )));
        }
        let relation = relation.trim();
        let is_call = (relation.starts_with("ref(") || relation.starts_with("source("))
            && relation.ends_with(')');
        if !is_call {
            return Err(Error::new(format!(
                "{}: snapshot {name:?}: relation must be a ref() or source() call, not {relation:?}",
                file.path
            )));
        }
        let source =
            format!("{{% snapshot {name} %}}select * from {{{{ {relation} }}}}{{% endsnapshot %}}");
        session.env.add_template_owned(
            format!("{}.snapshot_{name}", file.package),
            scoped(&file.package, &source),
            Some(file.path.clone()),
        )?;
        snapshots.push((name, file));
    }
    Ok(snapshots)
}
