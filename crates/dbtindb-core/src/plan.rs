//! The statements that rebuild a project's relations.

use crate::{CompiledModel, Error, Materialization};

/// What already exists where a model is about to be built.
#[derive(Clone, Debug)]
pub struct Existing {
    pub kind: ExistingKind,
    /// Whether dbtindb created it, according to the database's
    /// `dbtindb_managed` table.
    pub managed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExistingKind {
    View,
    Table,
    /// Anything else (a materialized view, sequence, index, ...), named.
    Other(String),
}

/// One statement of a build, tagged with the model it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Statement {
    Drop { model: String, sql: String },
    Create { model: String, sql: String },
}

impl Statement {
    pub fn sql(&self) -> &str {
        match self {
            Statement::Drop { sql, .. } | Statement::Create { sql, .. } => sql,
        }
    }
}

/// Plans a full rebuild of `models` (in `order`, from [`crate::build_order`]).
///
/// Existing model relations are dropped first, dependents before their
/// dependencies, then everything is created in dependency order. Drops never
/// cascade. A relation that depends on a model but is not itself a model
/// makes the drop fail, and with the build running in one transaction
/// nothing changes. It is not silently dropped. Likewise a relation that
/// stands where a model should go but that dbtindb did not create is an
/// error, never dropped.
pub fn plan(
    models: &[CompiledModel],
    order: &[usize],
    mut existing: impl FnMut(&CompiledModel) -> Result<Option<Existing>, Error>,
) -> Result<Vec<Statement>, Error> {
    let mut drops = Vec::new();
    for &i in order.iter().rev() {
        let m = &models[i];
        let Some(found) = existing(m)? else { continue };
        if !found.managed {
            return Err(Error::new(format!(
                "{} already exists and was not created by dbtindb; drop or rename it, or rename model {:?}",
                m.relation, m.name
            )));
        }
        let kind = match &found.kind {
            ExistingKind::View => "VIEW",
            ExistingKind::Table => "TABLE",
            ExistingKind::Other(what) => {
                return Err(Error::new(format!(
                    "{} is a {what}, which dbtindb does not replace",
                    m.relation
                )));
            }
        };
        drops.push(Statement::Drop {
            model: m.name.clone(),
            sql: format!("DROP {kind} {}", m.relation),
        });
    }

    let creates = order.iter().map(|&i| {
        let m = &models[i];
        let kind = match m.materialized {
            Materialization::View => "VIEW",
            Materialization::Table => "TABLE",
        };
        Statement::Create {
            model: m.name.clone(),
            sql: format!("CREATE {kind} {} AS\n{}", m.relation, m.sql),
        }
    });
    Ok(drops.into_iter().chain(creates).collect())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn model(name: &str, m: Materialization) -> CompiledModel {
        CompiledModel {
            name: name.into(),
            path: format!("models/{name}.sql"),
            relation: format!("\"s\".\"{name}\""),
            sql: format!("select '{name}'"),
            materialized: m,
            depends_on: BTreeSet::new(),
            sources: BTreeSet::new(),
        }
    }

    #[test]
    fn drops_in_reverse_then_creates_in_order() {
        let models = vec![
            model("a", Materialization::Table),
            model("b", Materialization::View),
        ];
        let plan = plan(&models, &[0, 1], |m| {
            Ok(Some(Existing {
                kind: if m.name == "a" {
                    ExistingKind::Table
                } else {
                    ExistingKind::View
                },
                managed: true,
            }))
        })
        .unwrap();
        let sql: Vec<&str> = plan.iter().map(Statement::sql).collect();
        assert_eq!(
            sql,
            [
                r#"DROP VIEW "s"."b""#,
                r#"DROP TABLE "s"."a""#,
                "CREATE TABLE \"s\".\"a\" AS\nselect 'a'",
                "CREATE VIEW \"s\".\"b\" AS\nselect 'b'",
            ]
        );
    }

    #[test]
    fn refuses_to_replace_relations_it_did_not_create() {
        let models = vec![model("a", Materialization::Table)];
        let err = plan(&models, &[0], |_| {
            Ok(Some(Existing {
                kind: ExistingKind::Table,
                managed: false,
            }))
        })
        .unwrap_err();
        assert!(err.0.contains("was not created by dbtindb"), "{err}");

        let err = plan(&models, &[0], |_| {
            Ok(Some(Existing {
                kind: ExistingKind::Other("materialized view".into()),
                managed: true,
            }))
        })
        .unwrap_err();
        assert!(err.0.contains("is a materialized view"), "{err}");
    }
}
