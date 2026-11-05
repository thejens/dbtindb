//! The models, seeds, snapshots, tests and docs of installed packages,
//! and unit tests' fixture files and macro overrides.

use crate::parse_tests::load;
use crate::{Dialect, Materialized, NodeKind, ResourceType, Session};

/// A package in the shape of the modelling packages (dbt_project_evaluator,
/// the Fivetran ones): models over a source, a seed, a snapshot, its own
/// macros, tests, docs, config and hooks.
const AUDIT: &[(&str, &str)] = &[
    (
        "dbt_packages/audit/dbt_project.yml",
        "name: audit\n\
         models:\n  audit:\n    +materialized: table\n    staging:\n      +materialized: view\n\
         on-run-end: \"{{ audit_hook() }}\"\n",
    ),
    (
        "dbt_packages/audit/models/staging/stg_events.sql",
        "select id, {{ label() }} as label from {{ source('raw', 'events') }}",
    ),
    (
        "dbt_packages/audit/models/events.sql",
        "select * from {{ ref('stg_events') }} join {{ ref('kinds') }} using (id)",
    ),
    (
        "dbt_packages/audit/models/schema.yml",
        "sources:\n  - name: raw\n    tables: [{name: events}]\n\
         models:\n  - name: events\n    description: \"{{ doc('events') }}\"\n\
         \x20   columns: [{name: id, tests: [not_null, is_even]}]\n",
    ),
    (
        "dbt_packages/audit/models/docs.md",
        "{% docs events %}Audit events{% enddocs %}",
    ),
    ("dbt_packages/audit/seeds/kinds.csv", "id,kind\n1,a\n"),
    (
        "dbt_packages/audit/snapshots/snap.sql",
        "{% snapshot events_snapshot %}{{ config(unique_key='id', strategy='check', check_cols='all') }}\
         select * from {{ ref('events') }}{% endsnapshot %}",
    ),
    (
        "dbt_packages/audit/macros/label.sql",
        "{% macro label() %}'audit'{% endmacro %}{% macro audit_hook() %}select 'done'{% endmacro %}",
    ),
    (
        "dbt_packages/audit/tests/generic/is_even.sql",
        "{% test is_even(model, column_name) %}select * from {{ model }} where {{ column_name }} % 2 = 1{% endtest %}",
    ),
];

fn session(extra: &[(&str, &str)]) -> Session {
    let files: Vec<_> = AUDIT.iter().chain(extra).copied().collect();
    load(&files, Dialect::Postgres).unwrap()
}

#[test]
fn package_nodes_are_namespaced_and_configured() {
    let s = session(&[
        (
            "dbt_project.yml",
            "name: shop\nmodels:\n  audit:\n    +schema: audit\n",
        ),
        (
            "models/report.sql",
            "select * from {{ ref('audit', 'events') }}",
        ),
    ]);
    let nodes = &s.manifest.nodes;
    let events = &nodes["model.audit.events"];
    assert_eq!(events.package, "audit");
    assert_eq!(events.fqn, ["audit", "events"]);
    // The package's own config, then the root project's under its name.
    assert_eq!(events.materialized(), Some(Materialized::Table));
    assert_eq!(
        events.relation.as_ref().unwrap().rendered,
        r#""analytics_audit"."events""#
    );
    assert_eq!(
        nodes["model.audit.stg_events"].materialized(),
        Some(Materialized::View)
    );
    assert_eq!(
        events.depends_on.iter().collect::<Vec<_>>(),
        ["model.audit.stg_events", "seed.audit.kinds"]
    );
    assert_eq!(events.description, "Audit events");
    assert!(nodes.contains_key("snapshot.audit.events_snapshot"));
    assert_eq!(
        nodes["model.shop.report"]
            .depends_on
            .iter()
            .collect::<Vec<_>>(),
        ["model.audit.events"]
    );
    // The package's tests, its own generic test among them.
    let tests: Vec<_> = nodes
        .values()
        .filter(|n| n.resource_type == ResourceType::Test)
        .map(|n| n.unique_id.as_str())
        .collect();
    assert_eq!(
        tests,
        [
            "test.audit.is_even_events_id",
            "test.audit.not_null_events_id"
        ]
    );
    // Package hooks run first, in their package.
    assert_eq!(s.manifest.on_run_end.len(), 1);
    assert_eq!(
        s.render_str("<hook>", &s.manifest.on_run_end[0]).unwrap(),
        "select 'done'"
    );
}

#[test]
fn package_models_call_their_own_macros() {
    // The root project's `label` does not replace the package's inside
    // the package's models, as in dbt.
    let s = session(&[(
        "macros/label.sql",
        "{% macro label() %}'root'{% endmacro %}",
    )]);
    let sql = s.compile("model.audit.stg_events", false).unwrap();
    assert!(sql.contains("'audit' as label"), "{sql}");
}

#[test]
fn ref_resolves_in_the_root_project_then_the_package() {
    let s = session(&[
        ("dbt_project.yml", "name: shop\n"),
        // Its own relation name, or it would collide with the package's.
        (
            "models/stg_events.sql",
            "{{ config(alias='my_stg_events') }}select 1 as id",
        ),
        ("models/mine.sql", "select * from {{ ref('stg_events') }}"),
    ]);
    let nodes = &s.manifest.nodes;
    // The package's own model refs the root project's model of that name,
    // as dbt resolves bare refs in the root project first.
    assert!(
        nodes["model.audit.events"]
            .depends_on
            .contains("model.shop.stg_events")
    );
    assert!(
        nodes["model.shop.mine"]
            .depends_on
            .contains("model.shop.stg_events")
    );
    let err = load(
        &[
            ("models/a.sql", "select * from {{ ref('nope', 'events') }}"),
            ("dbt_packages/audit/dbt_project.yml", "name: audit\n"),
        ],
        Dialect::Postgres,
    )
    .err()
    .unwrap();
    assert!(err.0.contains("in package \"nope\""), "{err}");
}

#[test]
fn package_relations_must_not_collide() {
    let err = load(
        &[
            ("models/events.sql", "select 1"),
            ("dbt_packages/audit/dbt_project.yml", "name: audit\n"),
            ("dbt_packages/audit/models/events.sql", "select 2"),
        ],
        Dialect::Postgres,
    )
    .err()
    .unwrap();
    assert!(err.0.contains("would both be built as"), "{err}");
}

/// A model with a unit test whose inputs and expectation come from
/// fixture files and whose clock macro is overridden.
const UNIT: &[(&str, &str)] = &[
    (
        "models/orders.sql",
        "select id, {{ dbt.current_timestamp() }} as at, {{ stamp() }} as s from {{ ref('raw_orders') }}",
    ),
    ("models/raw_orders.sql", "select 1 as id"),
    ("macros/stamp.sql", "{% macro stamp() %}now(){% endmacro %}"),
    ("tests/fixtures/raw.sql", "select 1 as id"),
    ("tests/fixtures/expected.csv", "id\n1\n"),
    (
        "models/unit.yml",
        "unit_tests:\n  - name: t\n    model: orders\n\
         \x20   given:\n      - input: ref('raw_orders')\n        format: sql\n        fixture: raw\n\
         \x20   expect: {format: csv, fixture: expected}\n\
         \x20   overrides:\n      macros:\n        stamp: \"'fixed'\"\n        dbt.current_timestamp: \"'2024-01-01'\"\n",
    ),
];

#[test]
fn unit_tests_read_fixture_files_and_override_macros() {
    let s = load(UNIT, Dialect::Postgres).unwrap();
    let node = &s.manifest.nodes["unit_test.dbtindb.orders.t"];
    let NodeKind::UnitTest(def) = &node.kind else {
        panic!("not a unit test");
    };
    assert_eq!(
        def.expect,
        crate::project::unit_tests::Rows::Csv("id\n1\n".into())
    );
    let sql = s.compile_unit_test(node).unwrap();
    assert!(sql.contains("select 1 as id"), "{sql}");
    assert!(sql.contains("'2024-01-01' as at, 'fixed' as s"), "{sql}");
    // Only the unit test sees the overrides.
    let sql = s.compile("model.dbtindb.orders", false).unwrap();
    assert!(sql.contains("now() as s"), "{sql}");
}

#[test]
fn unit_test_overrides_must_name_macros() {
    let files: Vec<_> = UNIT
        .iter()
        .map(|&(p, t)| {
            if p == "models/unit.yml" {
                (p, "unit_tests:\n  - name: t\n    model: orders\n    given: []\n    expect: {rows: []}\n    overrides: {macros: {nope: 1}}\n")
            } else {
                (p, t)
            }
        })
        .collect();
    let s = load(&files, Dialect::Postgres).unwrap();
    let node = &s.manifest.nodes["unit_test.dbtindb.orders.t"];
    let err = s.compile_unit_test(node).unwrap_err();
    assert!(err.0.contains("\"nope\", which is not a macro"), "{err}");
}
