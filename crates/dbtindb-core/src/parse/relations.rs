//! Naming the relations nodes build: schema and identifier, through
//! `generate_schema_name` and `generate_alias_name`.

use std::collections::BTreeMap;

use minijinja::Value;

use crate::Error;
use crate::context::Relation;
use crate::node::{Materialized, Node, ResourceType};
use crate::session::Session;

/// Gives every relation-building node its schema and identifier, through
/// the project's (or the built-in) `generate_schema_name` and
/// `generate_alias_name` macros.
pub(super) fn resolve_relations(
    session: &Session,
    nodes: &mut BTreeMap<String, Node>,
) -> Result<(), Error> {
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
            if let Some(t) = names.get_mut(&(node.package.clone(), node.name.clone())) {
                t.ephemeral = true;
            }
            continue;
        }
        let node_value = Value::from_serialize(node.to_json());
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
                .call_macro(
                    &session.macro_for(&node.package, "generate_schema_name"),
                    &[custom_schema, node_value.clone()],
                )?
                .trim()
                .to_owned(),
        };
        let mut alias = session
            .call_macro(
                &session.macro_for(&node.package, "generate_alias_name"),
                &[custom_alias, node_value],
            )?
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
            if let Some(t) = names.get_mut(&(node.package.clone(), node.name.clone())) {
                t.relation = relation.clone();
            }
        }
        node.relation = Some(relation);
    }
    Ok(())
}
