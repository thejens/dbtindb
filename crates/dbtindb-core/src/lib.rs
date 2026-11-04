//! Database-agnostic core of dbtindb: dbt-style models, compiled inside the
//! database.
//!
//! A project is a set of named template files, exactly the rows of the
//! `jinja_templates` table that pgjinja and sqlite-jinja render from:
//!
//! - `models/**/<name>.sql` are models. Each is a SELECT that dbtindb
//!   materializes as a view or table called `<name>`.
//! - `macros/**/*.sql` hold macros, which every model can call without an
//!   import, as in dbt.
//! - Anything else can be pulled in explicitly with `include`/`import`.
//!
//! Models are rendered with dbt's own Jinja dialect (dbt v2's MiniJinja fork)
//! and the dbt functions that shape a DAG: `ref`, `source`, `config`, `var`,
//! `this`, `target` and `is_incremental`. This crate compiles models,
//! orders them, and plans the statements that build them; the database
//! extensions execute the plan.

mod compile;
mod dialect;
mod error;
mod graph;
mod plan;
mod project;

pub use compile::{CompiledModel, Materialization, compile};
pub use dialect::Dialect;
pub use error::Error;
pub use graph::build_order;
pub use plan::{Existing, ExistingKind, Statement, plan};
pub use project::{Model, Project};

/// Where models are built and how SQL is spelled there.
#[derive(Clone, Debug)]
pub struct Target {
    pub dialect: Dialect,
    /// Schema that models are created in. SQLite ignores it: views there
    /// cannot reference other schemas, so models live in `main`.
    pub schema: String,
}
