//! What the database extensions call: loading a project, and the
//! `run`/`build`/`ls`/`compile` entry points with their options.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::node::{Materialized, NodeKind};
use crate::run::{self, Command, Options, Step};
use crate::session::{Session, Target};
use crate::{Database, Error, Project, RelationKind, graph, select};

/// Options of one call, as given to the SQL functions: either a selector
/// (`'orders+'`) or a JSON object
/// `{"select": ..., "exclude": ..., "vars": {...}, "full_refresh": true}`.
#[derive(Clone, Debug, Default)]
pub struct Invocation {
    pub options: Options,
    pub vars: Value,
}

impl Invocation {
    pub fn parse(arg: Option<&str>) -> Result<Self, Error> {
        let Some(arg) = arg.map(str::trim).filter(|a| !a.is_empty()) else {
            return Ok(Self::default());
        };
        if !arg.starts_with('{') {
            return Ok(Invocation {
                options: Options {
                    select: arg.to_owned(),
                    ..Options::default()
                },
                vars: Value::Null,
            });
        }
        let v: Value = serde_json::from_str(arg)
            .map_err(|e| Error::new(format!("options are not valid JSON: {e}")))?;
        let obj = v.as_object().expect("starts with {");
        let mut inv = Invocation::default();
        for (k, v) in obj {
            let text = |v: &Value| match v {
                Value::String(s) => Ok(s.clone()),
                Value::Array(items) => Ok(items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")),
                _ => Err(Error::new(format!(
                    "option {k} must be a string or a list of strings"
                ))),
            };
            match k.as_str() {
                "select" | "models" | "s" => inv.options.select = text(v)?,
                "exclude" => inv.options.exclude = text(v)?,
                "vars" => inv.vars = v.clone(),
                "full_refresh" => {
                    inv.options.full_refresh = v
                        .as_bool()
                        .ok_or_else(|| Error::new("full_refresh must be true or false"))?
                }
                other => {
                    return Err(Error::new(format!(
                        "unknown option {other:?} (select, exclude, vars, full_refresh)"
                    )));
                }
            }
        }
        Ok(inv)
    }
}

pub fn load(
    files: Vec<(String, String)>,
    target: &Target,
    vars: &Value,
    db: Arc<dyn Database>,
) -> Result<Session, Error> {
    Session::load(Project::new(files)?, target, vars, Some(db))
}

pub fn invoke(session: &Session, command: Command, options: &Options) -> Result<Vec<Step>, Error> {
    run::execute(session, command, options)
}

/// One listed node, for `ls`.
#[derive(Clone, Debug)]
pub struct Listed {
    pub build_order: usize,
    pub unique_id: String,
    pub name: String,
    pub resource_type: String,
    pub materialized: Option<String>,
    pub relation: Option<String>,
    pub path: String,
    pub depends_on: Vec<String>,
    pub sources: Vec<String>,
    pub tags: Vec<String>,
}

impl Listed {
    pub fn to_json(&self) -> Value {
        json!({
            "build_order": self.build_order,
            "unique_id": self.unique_id,
            "name": self.name,
            "resource_type": self.resource_type,
            "materialized": self.materialized,
            "relation": self.relation,
            "path": self.path,
            "depends_on": self.depends_on,
            "sources": self.sources,
            "tags": self.tags,
        })
    }
}

/// The selected nodes in build order, with their lineage, like `dbt ls`.
pub fn ls(session: &Session, options: &Options) -> Result<Vec<Listed>, Error> {
    let m = &session.manifest;
    let selected = select::select(m, &options.select, &options.exclude)?;
    let mut out = Vec::new();
    for id in graph::build_order(m)? {
        if !selected.contains(&id) {
            continue;
        }
        let n = &m.nodes[&id];
        let materialized = match &n.kind {
            NodeKind::Model(mat) => Some(mat.as_str().to_owned()),
            NodeKind::Seed => Some("seed".to_owned()),
            NodeKind::Snapshot => Some("snapshot".to_owned()),
            _ => None,
        };
        out.push(Listed {
            build_order: out.len() + 1,
            unique_id: id.clone(),
            name: n.name.clone(),
            resource_type: n.resource_type.as_str().to_owned(),
            materialized,
            relation: n
                .relation
                .as_ref()
                .filter(|_| n.materialized() != Some(Materialized::Ephemeral))
                .map(|r| r.rendered.clone()),
            path: n.path.clone(),
            depends_on: n.depends_on.iter().cloned().collect(),
            sources: n.sources.iter().map(|(s, t)| format!("{s}.{t}")).collect(),
            tags: n.config.tags()?,
        });
    }
    Ok(out)
}

/// The SQL a model, snapshot or test compiles to, as `dbt compile` shows
/// it. An incremental model compiles as it would run now: incrementally if
/// dbtindb built its table before.
pub fn compile(session: &Session, name: &str) -> Result<String, Error> {
    let m = &session.manifest;
    let node = m.nodes.values().find(|n| n.name == name).ok_or_else(|| {
        Error::new(format!(
            "no enabled model, snapshot, test or analysis named {name:?}"
        ))
    })?;
    if node.kind_is_seed() {
        return Err(Error::new(format!("{name:?} is a seed; it has no SQL")));
    }
    let incremental = match (&node.kind, &node.relation, &session.shared.db) {
        (NodeKind::Model(Materialized::Incremental), Some(rel), Some(db)) => {
            db.relation_kind(&rel.schema, &rel.identifier)? == Some(RelationKind::Table)
        }
        _ => false,
    };
    session.compile(&node.unique_id, incremental)
}

/// Ad-hoc SQL rendered against the project, like `dbt compile --inline`.
pub fn compile_inline(session: &Session, sql: &str) -> Result<String, Error> {
    session.compile_inline(sql)
}

/// Steps as JSON objects.
pub fn steps_json(steps: &[Step]) -> Value {
    Value::Array(
        steps
            .iter()
            .map(|s| {
                let mut o = Map::new();
                o.insert("step".into(), s.step.into());
                o.insert("unique_id".into(), s.unique_id.clone().into());
                o.insert("name".into(), s.name.clone().into());
                o.insert("resource_type".into(), s.resource_type.clone().into());
                o.insert("action".into(), s.action.clone().into());
                o.insert("status".into(), s.status.clone().into());
                o.insert("failures".into(), s.failures.into());
                o.insert("sql".into(), s.sql.clone().into());
                Value::Object(o)
            })
            .collect(),
    )
}
