//! Parsing: rendering every node once with `execute` false, to learn its
//! config, dependencies and relation, as dbt does before it runs anything.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::context::{Current, RefTarget, in_package};
use crate::node::{Manifest, Materialized, Node, NodeKind, ResourceType};
use crate::project::unit_tests::UnitTestDef;
use crate::project::{ColumnProps, NodeFile, ResourceProps};
use crate::session::Session;
use crate::{Error, testgen};

mod relations;
mod snapshots;

use relations::resolve_relations;
use snapshots::snapshot_names;

pub fn parse(session: &mut Session) -> Result<Manifest, Error> {
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();

    // Every name ref() can resolve, before any node renders. Snapshot names
    // come from their {% snapshot %} blocks.
    let snapshots = snapshot_names(session)?;
    let p = &session.project;
    let mut declared: Vec<(ResourceType, String, NodeFile)> = Vec::new();
    for (_, _, r) in p.all() {
        declared.extend(
            r.models
                .values()
                .map(|f| (ResourceType::Model, f.name.clone(), f.clone())),
        );
        declared.extend(
            r.seeds
                .values()
                .map(|f| (ResourceType::Seed, f.name.clone(), f.clone())),
        );
    }
    declared.extend(
        snapshots
            .into_iter()
            .map(|(n, f)| (ResourceType::Snapshot, n, f)),
    );
    {
        let mut names = session.shared.names.write().unwrap();
        for (rt, name, file) in &declared {
            let target = RefTarget {
                package: file.package.clone(),
                unique_id: unique_id(*rt, &file.package, name),
                relation: session.shared.relation(&session.shared.target_schema, name),
                ephemeral: false,
                enabled: true,
            };
            if let Some(other) = names.insert((file.package.clone(), name.clone()), target) {
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
    for (_, _, r) in p.all() {
        declared.extend(
            r.analyses
                .values()
                .map(|f| (ResourceType::Analysis, f.name.clone(), f.clone())),
        );
    }
    for (rt, name, file) in &declared {
        let props = p.resources(&file.package).map(|r| &r.properties);
        let props = props
            .and_then(|props| match rt {
                ResourceType::Model => props.models.get(name),
                ResourceType::Seed => props.seeds.get(name),
                ResourceType::Analysis => props.analyses.get(name),
                _ => props.snapshots.get(name),
            })
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
                && let Some(t) = names.get_mut(&(node.package.clone(), node.name.clone()))
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
    for (package, _, r) in session.project.all() {
        for def in &r.properties.unit_tests {
            if let Some(node) = unit_test_node(session, &nodes, package, def)? {
                nodes.insert(node.unique_id.clone(), node);
            }
        }
    }
    // Tests that store their failures are named only now.
    resolve_relations(session, &mut nodes)?;

    // As in dbt, the packages' hooks run first, in package name order, and
    // the root project's last. Each runs in its own package.
    let (mut on_run_start, mut on_run_end) = (Vec::new(), Vec::new());
    for package in session.project.packages.values() {
        let s = &package.settings;
        on_run_start.extend(s.on_run_start.iter().map(|h| in_package(&package.name, h)));
        on_run_end.extend(s.on_run_end.iter().map(|h| in_package(&package.name, h)));
    }
    on_run_start.extend(session.project.settings.on_run_start.iter().cloned());
    on_run_end.extend(session.project.settings.on_run_end.iter().cloned());
    Ok(Manifest {
        nodes,
        on_run_start,
        on_run_end,
    })
}

/// A node's unique id, as in dbt: `<resource type>.<package>.<name>`.
pub fn unique_id(rt: ResourceType, package: &str, name: &str) -> String {
    format!("{}.{package}.{name}", rt.as_str())
}

fn parse_node(
    session: &Session,
    rt: ResourceType,
    name: &str,
    file: &NodeFile,
    props: &ResourceProps,
) -> Result<Node, Error> {
    let package = &file.package;
    let mut fqn = vec![package.clone()];
    fqn.extend(file.dirs.iter().cloned());
    fqn.push(name.to_owned());
    let mut config = project_config(session, rt, package, &fqn);
    config.merge(&props.config);
    let unique_id = unique_id(rt, package, name);
    let mut node = Node {
        unique_id: unique_id.clone(),
        resource_type: rt,
        name: name.to_owned(),
        package: package.clone(),
        path: file.path.clone(),
        fqn,
        config,
        kind: NodeKind::Seed,
        relation: None,
        depends_on: BTreeSet::new(),
        sources: BTreeSet::new(),
        description: render_description(session, package, &file.path, &props.description)?,
        columns: props
            .columns
            .iter()
            .map(|c| {
                Ok(ColumnProps {
                    description: render_description(session, package, &file.path, &c.description)?,
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
        _ => s
            .call_macro(&format!("{package}.snapshot_{name}"), &[])
            .map(|_| ()),
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
    check_unsupported_config(&node)?;
    Ok(node)
}

/// Configs that change what dbt builds but that dbtindb does not
/// implement. Ignoring them would build something else than dbt does, so
/// they are errors.
const UNSUPPORTED_CONFIG: &[(&str, &str)] = &[
    (
        "incremental_predicates",
        "incremental_predicates is not supported",
    ),
    (
        "dbt_valid_to_current",
        "dbt_valid_to_current is not supported; current snapshot rows have dbt_valid_to null",
    ),
    (
        "snapshot_meta_column_names",
        "snapshot_meta_column_names is not supported; snapshots use dbt's default column names",
    ),
];

fn check_unsupported_config(node: &Node) -> Result<(), Error> {
    for (key, why) in UNSUPPORTED_CONFIG {
        if node.config.get(key).is_some() {
            return Err(Error::new(format!("{}: {why}", node.path)));
        }
    }
    // dbt only checks constraints when a contract is enforced.
    let enforced = node
        .config
        .get("contract")
        .and_then(|c| c.get("enforced"))
        .is_some_and(|e| e == true);
    if enforced {
        return Err(Error::new(format!(
            "{}: enforced model contracts (contract: {{enforced: true}}) are not supported",
            node.path
        )));
    }
    Ok(())
}

/// The config `dbt_project.yml` gives a node of `package`: the package's
/// own config trees, then the root project's, which can configure the
/// package's nodes under its name.
pub fn project_config(
    session: &Session,
    rt: ResourceType,
    package: &str,
    fqn: &[String],
) -> Config {
    let tree = |s: &crate::project::Settings| match rt {
        ResourceType::Model => s.models.clone(),
        ResourceType::Seed => s.seeds.clone(),
        ResourceType::Snapshot => s.snapshots.clone(),
        ResourceType::Test => s.tests.clone(),
        _ => serde_json::Value::Null,
    };
    let root = &session.project.settings;
    let mut config = match session.project.packages.get(package) {
        Some(p) => Config::from_project(&tree(&p.settings), fqn),
        None => Config::default(),
    };
    config.merge(&Config::from_project(&tree(root), fqn).0);
    config
}

/// The node of a unit test declared in `package`, or `None` when it or its
/// model is disabled.
fn unit_test_node(
    session: &Session,
    nodes: &BTreeMap<String, Node>,
    package: &str,
    def: &UnitTestDef,
) -> Result<Option<Node>, Error> {
    let model = unique_id(ResourceType::Model, package, &def.model);
    if !nodes.contains_key(&model) {
        let disabled = session
            .shared
            .names
            .read()
            .unwrap()
            .get(&(package.to_owned(), def.model.clone()))
            .is_some_and(|t| t.unique_id == model && !t.enabled);
        if disabled {
            return Ok(None);
        }
        return Err(Error::new(format!(
            "{}: unit test {:?} tests {:?}, which is not a model",
            def.file, def.name, def.model
        )));
    }
    let mut config = Config::default();
    config.merge(&def.config);
    if !config.enabled()? {
        return Ok(None);
    }
    let file = session.project.declared_in(&def.file, &def.name);
    let mut fqn = vec![package.to_owned()];
    fqn.extend(file.dirs);
    fqn.extend([def.model.clone(), def.name.clone()]);
    Ok(Some(Node {
        unique_id: format!("unit_test.{package}.{}.{}", def.model, def.name),
        resource_type: ResourceType::UnitTest,
        name: def.name.clone(),
        package: package.to_owned(),
        path: def.file.clone(),
        fqn,
        config,
        kind: NodeKind::UnitTest(Box::new(def.clone())),
        relation: None,
        // Like a data test of the model: selected with it, and run before
        // anything downstream of it.
        depends_on: BTreeSet::from([model]),
        sources: BTreeSet::new(),
        description: render_description(session, package, &def.file, &def.description)?,
        columns: Vec::new(),
    }))
}

/// A description as dbt shows it: rendered in the node's package, so
/// `{{ doc('orders') }}` becomes the docs block's text.
pub fn render_description(
    session: &Session,
    package: &str,
    path: &str,
    text: &str,
) -> Result<String, Error> {
    if !text.contains("{{") && !text.contains("{%") {
        return Ok(text.to_owned());
    }
    session
        .render_str("<description>", &in_package(package, text))
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
        node.to_json(),
        node.config.clone(),
    );
    let result = render(session);
    let cur = session.shared.finish();
    result.map(|()| cur)
}

/// Property files that describe nodes that do not exist are almost always
/// typos; dbt warns, dbtindb refuses so the tests they declare are not
/// silently skipped.
fn check_unknown_properties(
    session: &Session,
    nodes: &BTreeMap<String, Node>,
) -> Result<(), Error> {
    for (package, _, r) in session.project.all() {
        let p = &r.properties;
        for (rt, entries) in [
            (ResourceType::Model, &p.models),
            (ResourceType::Seed, &p.seeds),
            (ResourceType::Snapshot, &p.snapshots),
            (ResourceType::Analysis, &p.analyses),
        ] {
            for (name, props) in entries {
                if !nodes.contains_key(&unique_id(rt, package, name)) {
                    return Err(Error::new(format!(
                        "{} describes {} {name:?}, which does not exist",
                        props.file,
                        rt.as_str()
                    )));
                }
            }
        }
    }
    Ok(())
}
