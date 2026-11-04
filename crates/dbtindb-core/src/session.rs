//! A loaded project: its Jinja environment, shared context and manifest.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use minijinja::{Environment, Value};
use serde_json::{Map, Value as Json};

use crate::context::{self, Current, Shared};
use crate::node::Manifest;
use crate::project::Project;
use crate::{Database, Dialect, Error, parse};

/// Where models are built.
#[derive(Clone, Debug)]
pub struct Target {
    pub dialect: Dialect,
    /// The default schema; SQLite always builds in `main`.
    pub schema: String,
}

/// The name of an internal template that is only ever used to get a
/// [`minijinja::State`] for calling macros from Rust.
const SCRATCH: &str = "<dbtindb>";

pub struct Session {
    pub project: Project,
    pub env: Environment<'static>,
    pub shared: Arc<Shared>,
    pub manifest: Manifest,
}

impl Session {
    /// Parses `project`. `db` is used while executing; parsing itself runs
    /// no queries.
    pub fn load(
        project: Project,
        target: &Target,
        vars: &Json,
        db: Option<Arc<dyn Database>>,
    ) -> Result<Session, Error> {
        let mut all_vars = project.settings.vars.clone();
        match vars {
            Json::Null => {}
            Json::Object(m) => all_vars.extend(m.clone()),
            other => {
                return Err(Error::new(format!(
                    "vars must be a JSON object, got {other}"
                )));
            }
        }
        let database_name = db
            .as_ref()
            .map(|d| d.database_name())
            .unwrap_or_else(|| "main".into());
        let shared = Arc::new(Shared {
            dialect: target.dialect,
            project_name: project.settings.name.clone(),
            target_schema: target.schema.clone(),
            database_name,
            vars: all_vars,
            db,
            names: RwLock::new(BTreeMap::new()),
            sources: project.properties.sources.clone(),
            current: Mutex::new(Current::default()),
        });
        let mut env = context::environment(&project, shared.clone())?;
        env.add_template_owned(SCRATCH.to_owned(), String::new(), None)?;
        let mut session = Session {
            project,
            env,
            shared,
            manifest: Manifest::default(),
        };
        session.manifest = parse::parse(&mut session)?;
        Ok(session)
    }

    /// Calls a macro by name (looked up in dbt's global namespace) and
    /// returns what it renders or `return()`s, as text.
    pub fn call_macro(&self, name: &str, args: &[Value]) -> Result<String, Error> {
        let template = self.env.get_template(SCRATCH)?;
        let state = template.eval_to_state((), &[])?;
        state.call_macro(name, args, &[]).map_err(Error::from)
    }

    /// Whether a macro of this name exists, in the project or built in.
    pub fn has_macro(&self, name: &str) -> bool {
        let Ok(template) = self.env.get_template(SCRATCH) else {
            return false;
        };
        template
            .eval_to_state((), &[])
            .map(|s| s.lookup(name, &[]).is_some())
            .unwrap_or(false)
    }

    /// Renders a template file with the context of the current node.
    pub fn render_file(&self, path: &str) -> Result<String, Error> {
        Ok(self.env.get_template(path)?.render((), &[])?)
    }

    /// Renders a template given as text (hooks, test arguments).
    pub fn render_str(&self, name: &str, source: &str) -> Result<String, Error> {
        Ok(self.env.render_named_str(name, source, (), &[])?)
    }

    pub fn node_json(&self, unique_id: &str) -> Map<String, Json> {
        self.manifest.nodes[unique_id].to_json(&self.shared.project_name)
    }
}
