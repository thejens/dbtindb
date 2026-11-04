//! Parsing: rendering every node once with `execute` false, to learn its
//! config, dependencies and relation, as dbt does before it runs anything.

use std::collections::{BTreeMap, BTreeSet};

use minijinja::Value;

use crate::config::Config;
use crate::context::{Current, RefTarget, Relation};
use crate::node::{Manifest, Materialized, Node, NodeKind, ResourceType};
use crate::project::{ColumnProps, NodeFile, ResourceProps};
use crate::session::Session;
use crate::{Error, testgen};

pub fn parse(session: &mut Session) -> Result<Manifest, Error> {
    let project_name = session.shared.project_name.clone();
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();

    // Every name ref() can resolve, before any node renders. Snapshot names
    // come from their {% snapshot %} blocks.
    let mut snapshots = Vec::new();
    for file in &session.project.snapshot_files {
        let source = session.project.files[&file.path].clone();
        for name in crate::context::macro_names(&mut session.env, &file.path, &source)? {
            if let Some(snapshot) = name.strip_prefix("snapshot_") {
                snapshots.push((snapshot.to_owned(), file.clone()));
                // Callable by name through dbt's global macro namespace.
                session.env.add_template_owned(
                    format!("{project_name}.{name}"),
                    source.clone(),
                    Some(file.path.clone()),
                )?;
            }
        }
    }
    // dbt 1.9's YAML snapshots name a `relation:` instead of a SELECT.
    let yaml_snapshots: Vec<(String, String, NodeFile)> = session
        .project
        .properties
        .snapshots
        .iter()
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
        if let Some((_, sql)) = snapshots.iter().find(|(n, _)| *n == name) {
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
        session.env.add_template_owned(
            format!("{project_name}.snapshot_{name}"),
            format!("{{% snapshot {name} %}}select * from {{{{ {relation} }}}}{{% endsnapshot %}}"),
            Some(file.path.clone()),
        )?;
        snapshots.push((name, file));
    }

    let p = &session.project;
    let mut declared: Vec<(ResourceType, String, NodeFile)> = Vec::new();
    declared.extend(
        p.models
            .values()
            .map(|f| (ResourceType::Model, f.name.clone(), f.clone())),
    );
    declared.extend(
        p.seeds
            .values()
            .map(|f| (ResourceType::Seed, f.name.clone(), f.clone())),
    );
    declared.extend(
        snapshots
            .into_iter()
            .map(|(n, f)| (ResourceType::Snapshot, n, f)),
    );
    {
        let mut names = session.shared.names.write().unwrap();
        for (rt, name, file) in &declared {
            if *rt == ResourceType::Analysis {
                continue;
            }
            let unique_id = format!("{}.{project_name}.{name}", rt.as_str());
            let target = RefTarget {
                unique_id,
                relation: session.shared.relation(&session.shared.target_schema, name),
                ephemeral: false,
                enabled: true,
            };
            if let Some(other) = names.insert(name.clone(), target) {
                return Err(Error::new(format!(
                    "{name:?} names both {} and the {} in {}; ref() must be unambiguous",
                    other.unique_id,
                    rt.as_str(),
                    file.path
                )));
            }
        }
    }

    // Analyses can ref models but are not ref()able themselves.
    declared.extend(
        p.analyses
            .values()
            .map(|f| (ResourceType::Analysis, f.name.clone(), f.clone())),
    );
    for (rt, name, file) in &declared {
        let props = match rt {
            ResourceType::Model => p.properties.models.get(name),
            ResourceType::Seed => p.properties.seeds.get(name),
            ResourceType::Analysis => p.properties.analyses.get(name),
            _ => p.properties.snapshots.get(name),
        }
        .cloned()
        .unwrap_or_default();
        let node = parse_node(session, *rt, name, file, &props)?;
        nodes.insert(node.unique_id.clone(), node);
    }
    check_unknown_properties(session, &nodes)?;

    // Disabled nodes are dropped, and ref() to them becomes an error.
    {
        let mut names = session.shared.names.write().unwrap();
        for node in nodes.values() {
            if !node.config.enabled()?
                && let Some(t) = names.get_mut(&node.name)
            {
                t.enabled = false;
            }
        }
    }
    nodes.retain(|_, n| n.config.enabled().unwrap_or(true));
    for node in nodes.values() {
        for dep in &node.depends_on {
            if !nodes.contains_key(dep) {
                return Err(Error::new(format!(
                    "{} depends on {dep}, which is disabled (enabled=false)",
                    node.unique_id
                )));
            }
        }
    }

    resolve_relations(session, &mut nodes)?;

    let tests = testgen::tests(session, &nodes)?;
    for t in tests {
        if nodes.insert(t.unique_id.clone(), t).is_some() {
            unreachable!("test names are made unique");
        }
    }
    // Tests that store their failures are named only now.
    resolve_relations(session, &mut nodes)?;
    Ok(Manifest {
        nodes,
        on_run_start: session.project.settings.on_run_start.clone(),
        on_run_end: session.project.settings.on_run_end.clone(),
    })
}

fn parse_node(
    session: &Session,
    rt: ResourceType,
    name: &str,
    file: &NodeFile,
    props: &ResourceProps,
) -> Result<Node, Error> {
    let settings = &session.project.settings;
    let project_name = &settings.name;
    let mut fqn = vec![project_name.clone()];
    fqn.extend(file.dirs.iter().cloned());
    fqn.push(name.to_owned());
    let mut config = match rt {
        ResourceType::Model => Config::from_project(&settings.models, &fqn),
        ResourceType::Seed => Config::from_project(&settings.seeds, &fqn),
        ResourceType::Snapshot => Config::from_project(&settings.snapshots, &fqn),
        _ => Config::default(),
    };
    config.merge(&props.config);
    let unique_id = format!("{}.{project_name}.{name}", rt.as_str());
    let mut node = Node {
        unique_id: unique_id.clone(),
        resource_type: rt,
        name: name.to_owned(),
        path: file.path.clone(),
        fqn,
        config,
        kind: NodeKind::Seed,
        relation: None,
        depends_on: BTreeSet::new(),
        sources: BTreeSet::new(),
        description: render_description(session, &file.path, &props.description)?,
        columns: props
            .columns
            .iter()
            .map(|c| {
                Ok(ColumnProps {
                    description: render_description(session, &file.path, &c.description)?,
                    ..c.clone()
                })
            })
            .collect::<Result<_, Error>>()?,
    };
    if rt == ResourceType::Seed {
        return Ok(node);
    }

    let rendered = render_for_parse(session, &node, |s| match rt {
        ResourceType::Model | ResourceType::Analysis => s.render_file(&node.path).map(|_| ()),
        _ => s.call_macro(&format!("snapshot_{name}"), &[]).map(|_| ()),
    });
    let cur = rendered.map_err(|e| Error::new(format!("parsing {}: {e}", node.path)))?;
    node.config = cur.config;
    node.sources = cur.sources;
    node.depends_on = cur.refs;
    node.depends_on.remove(&unique_id);
    node.kind = match rt {
        ResourceType::Model => {
            let m = node.config.str("materialized")?.unwrap_or("view");
            NodeKind::Model(Materialized::parse(m).ok_or_else(|| {
                Error::new(format!(
                    "{}: materialized={m:?} is not supported (use view, table, incremental, ephemeral or materialized_view)",
                    node.path
                ))
            })?)
        }
        ResourceType::Analysis => NodeKind::Analysis,
        _ => NodeKind::Snapshot,
    };
    if node.config.get("database").is_some() {
        return Err(Error::new(format!(
            "{}: config database is not supported; dbtindb builds in the database it runs in",
            node.path
        )));
    }
    Ok(node)
}

/// A description as dbt shows it: rendered, so `{{ doc('orders') }}`
/// becomes the docs block's text.
fn render_description(session: &Session, path: &str, text: &str) -> Result<String, Error> {
    if !text.contains("{{") && !text.contains("{%") {
        return Ok(text.to_owned());
    }
    session
        .render_str("<description>", text)
        .map_err(|e| Error::new(format!("{path}: rendering a description: {e}")))
}

/// Renders with `execute` false and returns what the render recorded.
pub fn render_for_parse(
    session: &Session,
    node: &Node,
    render: impl FnOnce(&Session) -> Result<(), Error>,
) -> Result<Current, Error> {
    let this = session
        .shared
        .relation(&session.shared.target_schema, &node.name);
    session.shared.begin(
        false,
        &node.unique_id,
        Some(this),
        node.to_json(&session.shared.project_name),
        node.config.clone(),
    );
    let result = render(session);
    let cur = session.shared.finish();
    result.map(|()| cur)
}

/// Gives every relation-building node its schema and identifier, through
/// the project's (or the built-in) `generate_schema_name` and
/// `generate_alias_name` macros.
fn resolve_relations(session: &Session, nodes: &mut BTreeMap<String, Node>) -> Result<(), Error> {
    let shared = session.shared.clone();
    let mut seen: BTreeMap<(String, String), String> = BTreeMap::new();
    for node in nodes.values_mut() {
        if let Some(rel) = &node.relation {
            if node.materialized() != Some(Materialized::Ephemeral) {
                seen.insert(
                    (rel.schema.clone(), rel.identifier.clone()),
                    node.unique_id.clone(),
                );
            }
            continue;
        }
        if node.resource_type == ResourceType::Analysis
            || (node.is_test() && node.stored_failures()?.is_none())
        {
            continue;
        }
        if node.materialized() == Some(Materialized::Ephemeral) {
            node.relation = Some(Relation::cte(shared.dialect, &node.name));
            let mut names = shared.names.write().unwrap();
            if let Some(t) = names.get_mut(&node.name) {
                t.ephemeral = true;
            }
            continue;
        }
        let node_value = Value::from_serialize(node.to_json(&shared.project_name));
        let custom_schema = match node.config.str("schema")? {
            Some(s) => Value::from(s),
            // dbt keeps failing rows in `<target schema>_dbt_test__audit`;
            // SQLite has only the one schema.
            None if node.is_test() && shared.dialect.has_schemas() => {
                Value::from("dbt_test__audit")
            }
            None => Value::from(()),
        };
        let custom_alias = node
            .config
            .str("alias")?
            .map(Value::from)
            .unwrap_or(Value::from(()));
        // Snapshots configured the pre-1.9 way name their schema directly.
        let schema = match node.config.str("target_schema")? {
            Some(s) if node.resource_type == ResourceType::Snapshot => s.to_owned(),
            _ => session
                .call_macro("generate_schema_name", &[custom_schema, node_value.clone()])?
                .trim()
                .to_owned(),
        };
        let mut alias = session
            .call_macro("generate_alias_name", &[custom_alias, node_value])?
            .trim()
            .to_owned();
        // Generic test names grow with their arguments; dbt shortens long
        // ones the same way.
        if node.is_test() && alias.len() >= 64 {
            let hash = crate::run::checksum(&alias);
            let head: String = alias.chars().take(30).collect();
            alias = format!("{head}_{hash}");
        }
        if !shared.dialect.has_schemas() && schema != shared.target_schema {
            return Err(Error::new(format!(
                "{}: SQLite has no schemas, so {} cannot be built in schema {schema:?}; remove its schema config",
                node.path, node.unique_id
            )));
        }
        if let Some(max) = shared.dialect.max_identifier_bytes() {
            for ident in [&schema, &alias] {
                if ident.len() > max {
                    return Err(Error::new(format!(
                        "{}: {ident:?} is longer than the database's {max}-byte identifier limit, \
                         and would be silently truncated",
                        node.path
                    )));
                }
            }
        }
        if let Some(other) = seen.insert((schema.clone(), alias.clone()), node.unique_id.clone()) {
            return Err(Error::new(format!(
                "{} and {} would both be built as {}",
                other,
                node.unique_id,
                shared.dialect.relation(&schema, &alias)
            )));
        }
        let relation = shared.relation(&schema, &alias);
        if !node.is_test() {
            let mut names = shared.names.write().unwrap();
            if let Some(t) = names.get_mut(&node.name) {
                t.relation = relation.clone();
            }
        }
        node.relation = Some(relation);
    }
    Ok(())
}

/// Property files that describe nodes that do not exist are almost always
/// typos; dbt warns, dbtindb refuses so the tests they declare are not
/// silently skipped.
fn check_unknown_properties(
    session: &Session,
    nodes: &BTreeMap<String, Node>,
) -> Result<(), Error> {
    let p = &session.project.properties;
    for (rt, entries) in [
        (ResourceType::Model, &p.models),
        (ResourceType::Seed, &p.seeds),
        (ResourceType::Snapshot, &p.snapshots),
        (ResourceType::Analysis, &p.analyses),
    ] {
        for (name, props) in entries {
            let id = format!("{}.{}.{name}", rt.as_str(), session.shared.project_name);
            if !nodes.contains_key(&id) {
                return Err(Error::new(format!(
                    "{} describes {} {name:?}, which does not exist",
                    props.file,
                    rt.as_str()
                )));
            }
        }
    }
    Ok(())
}
