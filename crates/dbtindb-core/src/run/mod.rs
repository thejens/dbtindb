//! Running a command (`run`, `seed`, `snapshot`, `test`, `build`) against
//! the database.
//!
//! Everything happens on the caller's connection, in its transaction, so a
//! failure anywhere leaves the database as it was. Relations are only ever
//! replaced if dbtindb created them, and never dropped with CASCADE: a
//! managed view that depends on a replaced relation is rebuilt along with
//! it, and anything else that depends on it makes the run fail.

mod managed;
mod materialize;
mod snapshot;
mod test;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use md5::{Digest, Md5};

use self::managed::Managed;
use crate::context::Relation;
use crate::node::{Materialized, Node, NodeKind, ResourceType};
use crate::session::Session;
use crate::{Database, Error, RelationKind, graph, select};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Run,
    Seed,
    Snapshot,
    Test,
    Build,
}

impl Command {
    fn runs(self, rt: ResourceType) -> bool {
        match self {
            Command::Run => rt == ResourceType::Model,
            Command::Seed => rt == ResourceType::Seed,
            Command::Snapshot => rt == ResourceType::Snapshot,
            Command::Test => rt == ResourceType::Test,
            Command::Build => true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub select: String,
    pub exclude: String,
    pub full_refresh: bool,
}

/// One executed node (or hook), as reported to the caller.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub step: usize,
    pub unique_id: String,
    pub name: String,
    pub resource_type: String,
    /// What was done: `create view`, `incremental (delete+insert)`, ...
    pub action: String,
    /// `success`, or for tests `pass`, `warn` or `fail`.
    pub status: String,
    /// For tests: the number of failing rows.
    pub failures: Option<i64>,
    /// The statements that ran, separated by `;\n`.
    pub sql: String,
}

/// What happens to a node's relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    /// Drop (if it exists) and create.
    Replace,
    /// Build into the existing relation: incremental, snapshot, seed reload
    /// or materialized view refresh.
    Update,
    /// Tests: no relation.
    Query,
}

pub fn execute(session: &Session, command: Command, options: &Options) -> Result<Vec<Step>, Error> {
    let db = session
        .shared
        .db
        .clone()
        .ok_or_else(|| Error::new("running needs a database connection"))?;
    let mut runner = Runner {
        s: session,
        db: db.clone(),
        managed: Managed::new(db),
        steps: Vec::new(),
    };
    runner.run(command, options)?;
    Ok(runner.steps)
}

struct Runner<'a> {
    s: &'a Session,
    db: Arc<dyn Database>,
    managed: Managed,
    steps: Vec<Step>,
}

impl Runner<'_> {
    fn run(&mut self, command: Command, options: &Options) -> Result<(), Error> {
        let m = &self.s.manifest;
        let order = graph::build_order(m)?;
        let selected = select::select(m, &options.select, &options.exclude)?;
        let mut plans: BTreeMap<String, Plan> = BTreeMap::new();
        for id in &order {
            let node = &m.nodes[id];
            if !selected.contains(id) || !command.runs(node.resource_type) {
                continue;
            }
            if node.materialized() == Some(Materialized::Ephemeral) {
                continue;
            }
            plans.insert(id.clone(), self.plan(node, options.full_refresh)?);
        }
        self.add_dependent_views(&mut plans)?;
        self.create_schemas(&plans)?;

        for hook in &m.on_run_start {
            self.hook(None, hook, "on-run-start")?;
        }
        for id in order.iter().rev() {
            if plans.get(id) == Some(&Plan::Replace) {
                self.drop(&m.nodes[id])?;
            }
        }
        for id in &order {
            if let Some(&plan) = plans.get(id) {
                self.build(&m.nodes[id], plan, command)?;
            }
        }
        for hook in &m.on_run_end {
            self.hook(None, hook, "on-run-end")?;
        }
        Ok(())
    }

    /// Decides how to build `node` from what exists in the database.
    fn plan(&self, node: &Node, full_refresh: bool) -> Result<Plan, Error> {
        let Some(rel) = &node.relation else {
            return Ok(Plan::Query);
        };
        let Some(kind) = self.existing(node, rel)? else {
            return Ok(Plan::Replace);
        };
        // `full_refresh: false` protects a relation from --full-refresh.
        let full_refresh = node.config.bool("full_refresh")?.unwrap_or(full_refresh);
        Ok(match &node.kind {
            NodeKind::Model(Materialized::Incremental)
                if kind == RelationKind::Table && !full_refresh =>
            {
                Plan::Update
            }
            NodeKind::Snapshot if kind == RelationKind::Table => Plan::Update,
            NodeKind::Seed if kind == RelationKind::Table && !full_refresh => {
                let seed = materialize::seed_data(self.s, node)?;
                let cols = self.db.columns(&rel.schema, &rel.identifier)?;
                let same = cols.len() == seed.columns.len()
                    && cols
                        .iter()
                        .zip(&seed.columns)
                        .all(|(c, (n, _))| c.name == *n);
                if same { Plan::Update } else { Plan::Replace }
            }
            NodeKind::Model(Materialized::MaterializedView)
                if kind == RelationKind::MaterializedView && !full_refresh =>
            {
                // Refreshing keeps dependents; only a changed query rebuilds.
                let sql = self.s.compile(&node.unique_id, false)?;
                if self.managed.checksum(rel)?.as_deref() == Some(checksum(&sql).as_str()) {
                    Plan::Update
                } else {
                    Plan::Replace
                }
            }
            _ => Plan::Replace,
        })
    }

    /// What exists at `rel`; an error if it is something dbtindb did not
    /// create.
    fn existing(&self, node: &Node, rel: &Relation) -> Result<Option<RelationKind>, Error> {
        let Some(kind) = self.db.relation_kind(&rel.schema, &rel.identifier)? else {
            return Ok(None);
        };
        if !self.managed.contains(rel)? {
            return Err(Error::new(format!(
                "{} already exists and was not created by dbtindb; drop or rename it, or rename {}",
                rel.rendered, node.unique_id
            )));
        }
        if kind.drop_keyword().is_none() {
            return Err(Error::new(format!(
                "{} is a {}, which dbtindb does not replace",
                rel.rendered,
                kind.describe()
            )));
        }
        Ok(Some(kind))
    }

    /// Views (and materialized views) built by dbtindb on top of a replaced
    /// relation would block its drop; they are rebuilt with it, as dbt's
    /// CASCADE would remove them, but without losing them.
    fn add_dependent_views(&self, plans: &mut BTreeMap<String, Plan>) -> Result<(), Error> {
        let m = &self.s.manifest;
        let children = graph::children(m);
        let mut work: Vec<String> = plans
            .iter()
            .filter(|(_, p)| **p == Plan::Replace)
            .map(|(id, _)| id.clone())
            .collect();
        let mut visited: BTreeSet<String> = BTreeSet::new();
        while let Some(id) = work.pop() {
            for &child in children.get(id.as_str()).into_iter().flatten() {
                if !visited.insert(child.to_owned()) {
                    continue;
                }
                let node = &m.nodes[child];
                match node.materialized() {
                    // CTEs are inlined, so their dependents depend on `id`.
                    Some(Materialized::Ephemeral) => work.push(child.to_owned()),
                    Some(Materialized::View | Materialized::MaterializedView) => {
                        let rel = node.relation.as_ref().expect("views have relations");
                        if plans.get(child) != Some(&Plan::Replace)
                            && self.existing(node, rel)?.is_some()
                        {
                            plans.insert(child.to_owned(), Plan::Replace);
                            work.push(child.to_owned());
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Creates the schemas the planned relations go into, before anything
    /// else runs (so on-run-start hooks may use them), as dbt does.
    fn create_schemas(&self, plans: &BTreeMap<String, Plan>) -> Result<(), Error> {
        let dialect = self.db.dialect();
        if !dialect.has_schemas() {
            return Ok(());
        }
        let schemas: BTreeSet<&str> = plans
            .keys()
            .filter_map(|id| self.s.manifest.nodes[id].relation.as_ref())
            .map(|r| r.schema.as_str())
            .collect();
        for schema in schemas {
            // Checked first: Postgres requires the CREATE privilege on the
            // database even for IF NOT EXISTS on an existing schema.
            if !self.db.schema_exists(schema)? {
                self.exec(&format!(
                    "CREATE SCHEMA IF NOT EXISTS {}",
                    dialect.quote(schema)
                ))?;
            }
        }
        Ok(())
    }

    fn drop(&mut self, node: &Node) -> Result<(), Error> {
        let rel = node.relation.as_ref().expect("only relations are dropped");
        let Some(kind) = self.db.relation_kind(&rel.schema, &rel.identifier)? else {
            return Ok(());
        };
        let keyword = kind.drop_keyword().expect("checked while planning");
        let sql = format!("DROP {keyword} {}", rel.rendered);
        self.exec(&sql).map_err(|e| {
            Error::new(format!(
                "{e}\nwhile replacing {}: something that dbtindb did not build depends on it; \
                 drop that first, or make it a model",
                node.unique_id
            ))
        })?;
        self.managed.forget(rel)?;
        Ok(())
    }

    fn build(&mut self, node: &Node, plan: Plan, command: Command) -> Result<(), Error> {
        let mut sql = Vec::new();
        for hook in node.config.strings("pre_hook")? {
            sql.push(self.render_and_run(Some(&node.unique_id), &hook)?);
        }
        let (action, status, failures) = match &node.kind {
            NodeKind::SingularTest | NodeKind::GenericTest { .. } => {
                let r = test::run(self, node)?;
                sql.push(r.sql);
                if r.status == "fail" && command == Command::Build {
                    return Err(Error::new(format!(
                        "test {} failed with {} failing rows; nothing was built\n{}",
                        node.name,
                        r.failures,
                        sql.join(";\n")
                    )));
                }
                ("test".to_owned(), r.status, Some(r.failures))
            }
            NodeKind::Snapshot => {
                let (action, stmts) = snapshot::build(self, node, plan)?;
                sql.extend(stmts);
                (action, "success".into(), None)
            }
            NodeKind::Seed => {
                let (action, stmts) = materialize::seed(self, node, plan)?;
                sql.extend(stmts);
                (action, "success".into(), None)
            }
            NodeKind::Model(m) => {
                let (action, stmts) = materialize::model(self, node, *m, plan)?;
                sql.extend(stmts);
                (action, "success".into(), None)
            }
        };
        for hook in node.config.strings("post_hook")? {
            sql.push(self.render_and_run(Some(&node.unique_id), &hook)?);
        }
        if let Some(rel) = &node.relation {
            sql.extend(materialize::persist_docs(self, node, rel)?);
        }
        self.steps.push(Step {
            step: self.steps.len() + 1,
            unique_id: node.unique_id.clone(),
            name: node.name.clone(),
            resource_type: node.resource_type.as_str().into(),
            action,
            status,
            failures,
            sql: sql.join(";\n"),
        });
        Ok(())
    }

    fn hook(&mut self, unique_id: Option<&str>, hook: &str, name: &str) -> Result<(), Error> {
        let sql = self.render_and_run(unique_id, hook)?;
        self.steps.push(Step {
            step: self.steps.len() + 1,
            unique_id: String::new(),
            name: name.into(),
            resource_type: "hook".into(),
            action: name.into(),
            status: "success".into(),
            failures: None,
            sql,
        });
        Ok(())
    }

    fn render_and_run(&self, unique_id: Option<&str>, hook: &str) -> Result<String, Error> {
        let sql = self.s.render_hook(unique_id, hook)?;
        if !sql.is_empty() {
            self.exec(&sql)?;
        }
        Ok(sql)
    }

    fn exec(&self, sql: &str) -> Result<(), Error> {
        self.db
            .execute(sql, &[])
            .map_err(|e| Error::new(format!("{e}\nwhile running:\n{sql}")))
    }
}

/// Identifies a compiled query, to tell whether a materialized view's
/// definition changed.
fn checksum(sql: &str) -> String {
    Md5::digest(sql.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
