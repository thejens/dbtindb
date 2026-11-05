//! `grants`: the privileges others hold on a built relation, kept equal to
//! the config on PostgreSQL. SQLite has no roles, so it ignores them.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::context::Relation;
use crate::node::Node;
use crate::{Dialect, Error};

use super::Runner;
use super::materialize::run;

/// Grants what the config lists and revokes every other privilege held by
/// someone other than the owner, as dbt does. A relation that was just
/// replaced starts with no grants, so the revokes only matter for
/// relations kept across builds (incremental models, snapshots).
pub fn apply(r: &Runner<'_>, node: &Node, rel: &Relation) -> Result<Vec<String>, Error> {
    let d = r.db.dialect();
    if d != Dialect::Postgres {
        return Ok(Vec::new());
    }
    let wanted = wanted(node)?;
    // dbt leaves privileges alone when the config grants nothing.
    if wanted.is_empty() || r.db.relation_kind(&rel.schema, &rel.identifier)?.is_none() {
        return Ok(Vec::new());
    }
    let held = current(r, rel)?;
    let mut stmts = Vec::new();
    for (privilege, grantee) in &held {
        let kept = wanted
            .get(privilege)
            .is_some_and(|gs| gs.iter().any(|g| &role_name(g) == grantee));
        if !kept {
            stmts.push(run(
                r,
                format!(
                    "REVOKE {privilege} ON {} FROM {}",
                    rel.rendered,
                    role_sql(d, grantee)
                ),
            )?);
        }
    }
    // Revoking only removes pairs that are not wanted, so `held` still
    // tells what is missing.
    for (privilege, grantees) in &wanted {
        let missing: Vec<&str> = grantees
            .iter()
            .filter(|g| !held.contains(&(privilege.clone(), role_name(g))))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            stmts.push(run(
                r,
                format!(
                    "GRANT {privilege} ON {} TO {}",
                    rel.rendered,
                    missing.join(", ")
                ),
            )?);
        }
    }
    Ok(stmts)
}

/// The config's grants, by lower-case privilege. Grantees are written into
/// the SQL as given, as dbt does, so `public` and quoted names work.
fn wanted(node: &Node) -> Result<BTreeMap<String, BTreeSet<String>>, Error> {
    let mut wanted = BTreeMap::new();
    let Some(grants) = node.config.get("grants") else {
        return Ok(wanted);
    };
    let err = |what: &str| Error::new(format!("{}: grants: {what}", node.path));
    let grants = grants
        .as_object()
        .ok_or_else(|| err("expected a mapping of privileges to grantees"))?;
    for (privilege, grantees) in grants {
        if privilege.is_empty() || !privilege.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(err(&format!("{privilege:?} is not a privilege")));
        }
        let grantees = match grantees {
            Value::Null => Vec::new(),
            Value::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        let grantees = grantees
            .into_iter()
            .map(|g| match g {
                Value::String(s) => Ok(s),
                other => Err(err(&format!("grantee {other} is not a role name"))),
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        // An empty list is how a project revokes a privilege a parent
        // config granted.
        if !grantees.is_empty() {
            wanted.insert(privilege.to_ascii_lowercase(), grantees);
        }
    }
    Ok(wanted)
}

/// The role a grantee as written names: PostgreSQL folds unquoted names to
/// lower case.
fn role_name(grantee: &str) -> String {
    match grantee.strip_prefix('"').and_then(|g| g.strip_suffix('"')) {
        Some(quoted) => quoted.replace("\"\"", "\""),
        None => grantee.to_lowercase(),
    }
}

/// A role name read from the catalog, as SQL. `public` is a keyword
/// there, not a role, and must stay unquoted.
fn role_sql(d: Dialect, role: &str) -> String {
    if role == "public" {
        role.to_owned()
    } else {
        d.quote(role)
    }
}

/// The (privilege, role) pairs held on the relation by anyone but its
/// owner. Read from the ACL itself, since `information_schema` leaves out
/// materialized views.
fn current(r: &Runner<'_>, rel: &Relation) -> Result<BTreeSet<(String, String)>, Error> {
    let d = r.db.dialect();
    let table = r.db.query(&format!(
        "SELECT lower(a.privilege_type), \
         CASE WHEN a.grantee = 0 THEN 'public' ELSE pg_get_userbyid(a.grantee)::text END \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace, \
         aclexplode(c.relacl) a \
         WHERE n.nspname = {} AND c.relname = {} AND a.grantee <> c.relowner",
        d.literal(&rel.schema),
        d.literal(&rel.identifier)
    ))?;
    table
        .rows
        .into_iter()
        .map(|row| match row.as_slice() {
            [Value::String(p), Value::String(g)] => Ok((p.clone(), g.clone())),
            _ => Err(Error::new(format!(
                "unexpected row reading the grants on {}: {row:?}",
                rel.rendered
            ))),
        })
        .collect()
}
