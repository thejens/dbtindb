//! Macro namespaces: the root project, installed packages and dbt's own
//! macros, and `adapter.dispatch` across them.
//!
//! The fork registers every macro as a template named `<package>.<macro>`
//! holding the whole file that defines it. An unqualified name resolves in
//! the package of the macro being run (the fork's `TARGET_PACKAGE_NAME`),
//! then the root project, then dbt's built-in macros, as in dbt.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use minijinja::constants::TARGET_PACKAGE_NAME;
use minijinja::dispatch_object::DispatchObject;
use minijinja::value::{Enumerator, Object};
use minijinja::{Environment, State, Value};

use super::{jinja_error, macro_names};

/// The package whose built-in macros dbt calls `dbt`.
pub const DBT: &str = "dbt";

/// Every package's macros, by package name.
#[derive(Debug, Default)]
pub struct Registry {
    pub macros: BTreeMap<String, BTreeSet<String>>,
    /// Which file defines each macro, to report duplicates.
    owners: BTreeMap<(String, String), String>,
}

impl Registry {
    /// Registers the macros `source` (at `path`) defines, in `package`.
    ///
    /// `scoped` sets the file's current package: unqualified names in its
    /// macros resolve in `package` first. The root project and dbt's own
    /// macros leave it unset, so the root project's overrides win, as in
    /// dbt; installed packages set it, so their internal calls stay
    /// internal.
    pub fn add_file(
        &mut self,
        env: &mut Environment<'static>,
        package: &str,
        path: &str,
        source: &str,
        scoped: bool,
    ) -> Result<(), crate::Error> {
        let source = if scoped {
            in_package(package, source)
        } else {
            source.to_owned()
        };
        for name in macro_names(env, path, &source)? {
            let key = (package.to_owned(), name.clone());
            if let Some(other) = self.owners.insert(key, path.to_owned()) {
                return Err(crate::Error::new(format!(
                    "macro {package}.{name} is defined in both {other} and {path}"
                )));
            }
            env.add_template_owned(
                format!("{package}.{name}"),
                source.clone(),
                Some(path.to_owned()),
            )?;
            self.macros
                .entry(package.to_owned())
                .or_default()
                .insert(name);
        }
        Ok(())
    }

    /// Makes each package callable by name: `dbt_utils.star(...)`.
    pub fn add_namespaces(&self, env: &mut Environment<'static>) -> Result<(), crate::Error> {
        for (package, names) in &self.macros {
            if env.get_global(package).is_some() {
                return Err(crate::Error::new(format!(
                    "package {package:?} has the name of a dbt context variable"
                )));
            }
            env.add_global(
                package.clone(),
                Value::from_object(Namespace {
                    package: package.clone(),
                    names: names.iter().cloned().collect(),
                }),
            );
        }
        Ok(())
    }
}

/// A template's source, set to run in `package`: unqualified macro names
/// in it resolve in that package first.
pub fn in_package(package: &str, source: &str) -> String {
    // On the first line and without a newline, so error line numbers
    // still match the file.
    format!("{{% set {TARGET_PACKAGE_NAME} = {package:?} %}}{source}")
}

/// A package by name: its macros are its attributes.
#[derive(Debug)]
struct Namespace {
    package: String,
    names: Vec<String>,
}

impl Object for Namespace {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let name = key.as_str()?;
        self.names
            .binary_search_by(|n| n.as_str().cmp(name))
            .ok()
            .map(|_| macro_in(&self.package, name))
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Values(self.names.iter().map(|n| Value::from(n.as_str())).collect())
    }
}

/// The macro `name` of `package`, run in that package.
pub fn macro_in(package: &str, name: &str) -> Value {
    Value::from_object(DispatchObject {
        macro_name: name.to_owned(),
        package_name: Some(package.to_owned()),
        strict: true,
        auto_execute: false,
        context: None,
    })
}

/// Whether `package` defines a macro called `name`.
pub fn defines(state: &State<'_, '_>, package: &str, name: &str) -> bool {
    state
        .env()
        .get_template(&format!("{package}.{name}"))
        .is_ok()
}

/// A macro by name as dbt resolves it: `pkg.name` in that package, a bare
/// name through the current package, the root project and dbt.
pub fn resolve(state: &State<'_, '_>, name: &str) -> Option<Value> {
    match name.split_once('.') {
        Some((package, name)) => defines(state, package, name).then(|| macro_in(package, name)),
        None => state.lookup(name, &[]),
    }
}

/// `adapter.dispatch(macro_name, macro_namespace)`: the first of
/// `<adapter>__<name>` and `default__<name>` found in the packages dbt
/// searches.
///
/// As in dbt: a `dispatch:` config for the namespace gives the packages to
/// search; an installed package is searched after the root project, so the
/// project can override its implementations; anything else, and no
/// namespace, resolves as a bare name does.
pub fn dispatch(
    state: &State<'_, '_>,
    dialect: &str,
    root: &str,
    orders: &BTreeMap<String, Vec<String>>,
    installed: &BTreeSet<String>,
    name: &str,
    namespace: Option<&str>,
) -> Result<Value, minijinja::Error> {
    if name.contains('.') {
        return Err(jinja_error(format!(
            "adapter.dispatch({name:?}): a macro name cannot contain \".\"; \
             pass the package as macro_namespace"
        )));
    }
    let search: Vec<Option<String>> = match namespace {
        Some(ns) => match orders.get(ns) {
            Some(order) => order.iter().cloned().map(Some).collect(),
            None if installed.contains(ns) => vec![Some(root.to_owned()), Some(ns.to_owned())],
            None => vec![None],
        },
        None => vec![None],
    };
    let mut tried = Vec::new();
    for package in &search {
        for prefix in [dialect, "default"] {
            let candidate = format!("{prefix}__{name}");
            match package {
                Some(package) => {
                    if defines(state, package, &candidate) {
                        return Ok(macro_in(package, &candidate));
                    }
                    tried.push(format!("{package}.{candidate}"));
                }
                None => {
                    if let Some(m) = state.lookup(&candidate, &[]) {
                        return Ok(m);
                    }
                    tried.push(candidate);
                }
            }
        }
    }
    Err(jinja_error(format!(
        "adapter.dispatch: no macro {name:?} in namespace {}; searched {}",
        namespace.unwrap_or("None"),
        tried.join(", ")
    )))
}
