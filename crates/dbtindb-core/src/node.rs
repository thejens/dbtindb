//! The parsed project: every node with its final config, relation and
//! dependencies.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::Config;
use crate::context::Relation;
use crate::project::ColumnProps;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceType {
    Model,
    Seed,
    Snapshot,
    Test,
}

impl ResourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceType::Model => "model",
            ResourceType::Seed => "seed",
            ResourceType::Snapshot => "snapshot",
            ResourceType::Test => "test",
        }
    }
}

/// How a model is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Materialized {
    View,
    Table,
    Incremental,
    Ephemeral,
    MaterializedView,
}

impl Materialized {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "view" => Materialized::View,
            "table" => Materialized::Table,
            "incremental" => Materialized::Incremental,
            "ephemeral" => Materialized::Ephemeral,
            "materialized_view" => Materialized::MaterializedView,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Materialized::View => "view",
            Materialized::Table => "table",
            Materialized::Incremental => "incremental",
            Materialized::Ephemeral => "ephemeral",
            Materialized::MaterializedView => "materialized_view",
        }
    }
}

/// What a test runs against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestTarget {
    Node(String),
    Source(String, String),
}

#[derive(Clone, Debug)]
pub enum NodeKind {
    Model(Materialized),
    Seed,
    /// A `{% snapshot %}` block, called as the macro `snapshot_<name>`.
    Snapshot,
    SingularTest,
    GenericTest {
        /// The test macro: `test_<name>`.
        macro_name: String,
        /// Arguments besides `model` and `column_name`.
        args: Map<String, Value>,
        target: TestTarget,
        column: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub struct Node {
    pub unique_id: String,
    pub resource_type: ResourceType,
    pub name: String,
    /// The file that defines it: the template, or for generic tests the
    /// property file that applies them.
    pub path: String,
    pub fqn: Vec<String>,
    pub config: Config,
    pub kind: NodeKind,
    /// The relation the node builds; `None` for tests and ephemeral models.
    pub relation: Option<Relation>,
    /// Unique ids of the nodes it depends on.
    pub depends_on: BTreeSet<String>,
    pub sources: BTreeSet<(String, String)>,
    pub description: String,
    pub columns: Vec<ColumnProps>,
}

impl Node {
    pub fn materialized(&self) -> Option<Materialized> {
        match self.kind {
            NodeKind::Model(m) => Some(m),
            _ => None,
        }
    }

    pub fn kind_is_seed(&self) -> bool {
        matches!(self.kind, NodeKind::Seed)
    }

    pub fn is_test(&self) -> bool {
        self.resource_type == ResourceType::Test
    }

    /// The node as Jinja sees it in `model` and in `generate_schema_name`'s
    /// `node` argument.
    pub fn to_json(&self, project: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("name".into(), self.name.clone().into());
        m.insert("unique_id".into(), self.unique_id.clone().into());
        m.insert("resource_type".into(), self.resource_type.as_str().into());
        m.insert("package_name".into(), project.into());
        m.insert("path".into(), self.path.clone().into());
        m.insert("original_file_path".into(), self.path.clone().into());
        m.insert("fqn".into(), self.fqn.clone().into());
        m.insert("config".into(), Value::Object(self.config.0.clone()));
        m.insert("tags".into(), self.config.tags().unwrap_or_default().into());
        m.insert("description".into(), self.description.clone().into());
        if let Some(rel) = &self.relation {
            m.insert("schema".into(), rel.schema.clone().into());
            m.insert("alias".into(), rel.identifier.clone().into());
            m.insert("database".into(), rel.database.clone().into());
        }
        m
    }
}

/// Every enabled node of a project, by unique id.
#[derive(Clone, Debug, Default)]
pub struct Manifest {
    pub nodes: BTreeMap<String, Node>,
    pub on_run_start: Vec<String>,
    pub on_run_end: Vec<String>,
}

impl Manifest {
    /// The unique id of the model, seed or snapshot called `name`.
    pub fn find(&self, name: &str) -> Option<&Node> {
        self.nodes.values().find(|n| !n.is_test() && n.name == name)
    }
}
