//! Data tests: generic tests applied in property files, and singular tests
//! in `tests/`.

use std::collections::{BTreeMap, BTreeSet};

use minijinja::Value;
use minijinja::value::Kwargs;
use serde_json::Value as Json;

use crate::Error;
use crate::config::Config;
use crate::node::{Node, NodeKind, ResourceType, TestTarget};
use crate::parse::render_for_parse;
use crate::project::{ColumnProps, TestDef};
use crate::session::Session;

/// A test application found in a property file, before it is parsed.
struct Pending<'a> {
    def: &'a TestDef,
    target: TestTarget,
    /// Name of the tested model/seed/snapshot, or `<source>_<table>`.
    target_name: String,
    column: Option<String>,
    file: &'a str,
}

pub fn tests(session: &Session, nodes: &BTreeMap<String, Node>) -> Result<Vec<Node>, Error> {
    let project = &session.project;
    let pkg = &session.shared.project_name;
    let mut pending = Vec::new();
    let props = &project.properties;
    for (rt, entries) in [
        (ResourceType::Model, &props.models),
        (ResourceType::Seed, &props.seeds),
        (ResourceType::Snapshot, &props.snapshots),
    ] {
        for (name, p) in entries {
            let uid = format!("{}.{pkg}.{name}", rt.as_str());
            // Tests of a disabled node are disabled with it.
            if !nodes.contains_key(&uid) {
                continue;
            }
            collect(
                &mut pending,
                &p.tests,
                &p.columns,
                TestTarget::Node(uid),
                name,
                &p.file,
            );
        }
    }
    for ((source, table), t) in &props.sources {
        let target = TestTarget::Source(source.clone(), table.clone());
        collect(
            &mut pending,
            &t.tests,
            &t.columns,
            target,
            &format!("source_{source}_{table}"),
            &t.file,
        );
    }

    let mut used: BTreeSet<String> = nodes.values().map(|n| n.name.clone()).collect();
    let mut out = Vec::new();
    for p in pending {
        let node = generic(session, nodes, &p, &mut used)?;
        if node.config.enabled()? {
            out.push(node);
        }
    }
    for file in project.singular_tests.values() {
        if !used.insert(file.name.clone()) {
            return Err(Error::new(format!(
                "{}: a test named {:?} already exists",
                file.path, file.name
            )));
        }
        let mut fqn = vec![pkg.clone()];
        fqn.extend(file.dirs.iter().cloned());
        fqn.push(file.name.clone());
        let mut node = test_node(
            session,
            &file.name,
            &file.path,
            fqn,
            Config::default(),
            NodeKind::SingularTest,
        );
        let cur = render_for_parse(session, &node, |s| s.render_file(&file.path).map(|_| ()))
            .map_err(|e| Error::new(format!("parsing {}: {e}", file.path)))?;
        node.config = cur.config;
        node.depends_on = cur.refs;
        node.sources = cur.sources;
        if node.config.enabled()? {
            out.push(node);
        }
    }
    Ok(out)
}

fn collect<'a>(
    pending: &mut Vec<Pending<'a>>,
    tests: &'a [TestDef],
    columns: &'a [ColumnProps],
    target: TestTarget,
    target_name: &str,
    file: &'a str,
) {
    for def in tests {
        pending.push(Pending {
            def,
            target: target.clone(),
            target_name: target_name.to_owned(),
            column: None,
            file,
        });
    }
    for col in columns {
        for def in &col.tests {
            pending.push(Pending {
                def,
                target: target.clone(),
                target_name: target_name.to_owned(),
                column: Some(col.name.clone()),
                file,
            });
        }
    }
}

fn generic(
    session: &Session,
    nodes: &BTreeMap<String, Node>,
    p: &Pending<'_>,
    used: &mut BTreeSet<String>,
) -> Result<Node, Error> {
    // `dbt_utils.at_least_one` is the `{% test at_least_one %}` block of
    // the dbt_utils package; a bare name is looked up as any macro is.
    let macro_name = match p.def.test.split_once('.') {
        Some((package, test)) => format!("{package}.test_{test}"),
        None => format!("test_{}", p.def.test),
    };
    if !session.has_macro(&macro_name) {
        return Err(Error::new(format!(
            "{}: there is no generic test named {:?}; define it with {{% test {} %}} in macros/ or tests/generic/, \
             or install the package that provides it",
            p.file, p.def.test, p.def.test
        )));
    }
    let base = p.def.name.clone().unwrap_or_else(|| {
        let mut name = format!("{}_{}", p.def.test, p.target_name);
        if let Some(col) = &p.column {
            name.push('_');
            name.push_str(col);
        }
        name.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    });
    let mut name = base.clone();
    let mut n = 1;
    while !used.insert(name.clone()) {
        if p.def.name.is_some() {
            return Err(Error::new(format!(
                "{}: a test named {name:?} already exists",
                p.file
            )));
        }
        n += 1;
        name = format!("{base}_{n}");
    }

    let pkg = &session.shared.project_name;
    let mut fqn = vec![pkg.clone()];
    // The property file's directories below its resource path, as dbt
    // places tests in the folder tree of dbt_project.yml.
    let parts: Vec<&str> = p.file.split('/').collect();
    if parts.len() > 2 {
        fqn.extend(parts[1..parts.len() - 1].iter().map(|d| d.to_string()));
    }
    fqn.push(name.clone());
    let mut config = Config::from_project(&session.project.settings.tests, &fqn);
    config.merge(&p.def.config);
    let kind = NodeKind::GenericTest {
        macro_name,
        args: p.def.args.clone(),
        target: p.target.clone(),
        column: p.column.clone(),
    };
    let mut node = test_node(session, &name, p.file, fqn, config, kind);
    match &p.target {
        TestTarget::Node(uid) => {
            node.depends_on.insert(uid.clone());
        }
        TestTarget::Source(s, t) => {
            node.sources.insert((s.clone(), t.clone()));
        }
    }
    let cur = render_for_parse(session, &node, |s| {
        let model = tested_relation(s, nodes, &p.target)?;
        call_test(s, &node, model).map(|_| ())
    })
    .map_err(|e| Error::new(format!("parsing test {name} in {}: {e}", p.file)))?;
    node.config = cur.config;
    node.depends_on.extend(cur.refs);
    node.sources.extend(cur.sources);
    Ok(node)
}

fn test_node(
    session: &Session,
    name: &str,
    path: &str,
    fqn: Vec<String>,
    config: Config,
    kind: NodeKind,
) -> Node {
    Node {
        unique_id: format!("test.{}.{name}", session.shared.project_name),
        resource_type: ResourceType::Test,
        name: name.to_owned(),
        path: path.to_owned(),
        fqn,
        config,
        kind,
        relation: None,
        depends_on: BTreeSet::new(),
        sources: BTreeSet::new(),
        description: String::new(),
        columns: Vec::new(),
    }
}

/// Renders a generic test's SQL: its macro called with `model`,
/// `column_name` and its arguments. String arguments that are `ref(...)`
/// or `source(...)` calls are evaluated to relations, as dbt does.
/// What a generic test tests: the relation of its node or source.
pub fn tested_relation(
    session: &Session,
    nodes: &BTreeMap<String, Node>,
    target: &TestTarget,
) -> Result<Value, Error> {
    match target {
        TestTarget::Node(uid) => Ok(nodes[uid]
            .relation
            .clone()
            .map_or_else(|| Value::from(""), |r| r.into_value())),
        TestTarget::Source(s, t) => session.eval(&format!("source({s:?}, {t:?})")),
    }
}

pub fn call_test(session: &Session, node: &Node, relation: Value) -> Result<String, Error> {
    let NodeKind::GenericTest {
        macro_name,
        args,
        column,
        ..
    } = &node.kind
    else {
        unreachable!("only generic tests are called");
    };
    // As in dbt, the test's `model` is the relation, or with a `where`
    // config, the subquery dbt's `get_where_subquery` makes of it.
    let model = session.call_macro_value("get_where_subquery", &[relation])?;
    let mut kwargs: Vec<(String, Value)> = vec![("model".into(), model)];
    if let Some(col) = column {
        kwargs.push(("column_name".into(), Value::from(col.as_str())));
    }
    for (k, v) in args {
        kwargs.push((k.clone(), eval_arg(session, v)?));
    }
    session.call_macro(macro_name, &[Value::from(Kwargs::from_iter(kwargs))])
}

fn eval_arg(session: &Session, v: &Json) -> Result<Value, Error> {
    if let Json::String(s) = v {
        let t = s.trim();
        if (t.starts_with("ref(") || t.starts_with("source(")) && t.ends_with(')') {
            return session.eval(t);
        }
    }
    Ok(Value::from_serialize(v))
}
