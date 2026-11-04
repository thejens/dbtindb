//! Parsing a whole project: config resolution, relations, tests.

use serde_json::json;

use crate::{Dialect, Error, Materialized, NodeKind, Project, ResourceType, Session, Target};

pub fn project(files: &[(&str, &str)]) -> Project {
    Project::new(files.iter().map(|(p, s)| (p.to_string(), s.to_string()))).unwrap()
}

pub fn load(files: &[(&str, &str)], dialect: Dialect) -> Result<Session, Error> {
    let schema = if dialect == Dialect::Postgres {
        "analytics"
    } else {
        "main"
    };
    Session::load(
        project(files),
        &Target {
            dialect,
            schema: schema.into(),
        },
        &json!({}),
        None,
    )
}

const SHOP: &[(&str, &str)] = &[
    (
        "dbt_project.yml",
        "name: shop\nvars:\n  min_amount: 0\nmodels:\n  shop:\n    +materialized: view\n    marts:\n      +materialized: table\n      +schema: marts\n",
    ),
    (
        "macros/cents.sql",
        "{% macro cents(c) %}({{ c }} * 100){% endmacro %}",
    ),
    (
        "macros/dollars.sql",
        "{% macro dollars(c) %}{{ cents(c) }} / 100{% endmacro %}",
    ),
    ("seeds/countries.csv", "code,name\nSE,Sweden\n"),
    (
        "models/staging/stg_orders.sql",
        "select id, {{ cents('amount') }} as amount_cents, country from {{ source('raw', 'orders') }}",
    ),
    (
        "models/staging/int_paid.sql",
        "{{ config(materialized='ephemeral') }} select * from {{ ref('stg_orders') }} where amount_cents > {{ var('min_amount') }}",
    ),
    (
        "models/marts/orders.sql",
        "select p.*, c.name from {{ ref('int_paid') }} p join {{ ref('countries') }} c on c.code = p.country",
    ),
    (
        "models/marts/schema.yml",
        "models:\n  - name: orders\n    columns:\n      - name: id\n        data_tests: [unique, not_null]\n      - name: country\n        data_tests:\n          - relationships: {to: \"ref('countries')\", field: code}\nsources:\n  - name: raw\n    schema: landing\n    tables:\n      - name: orders\n        columns:\n          - name: id\n            data_tests: [not_null]\n",
    ),
    (
        "snapshots/orders_snapshot.sql",
        "{% snapshot orders_snapshot %}{{ config(unique_key='id', strategy='check', check_cols='all') }}select * from {{ ref('stg_orders') }}{% endsnapshot %}",
    ),
    (
        "tests/assert_no_negative.sql",
        "select * from {{ ref('orders') }} where amount_cents < 0",
    ),
];

#[test]
fn parses_a_whole_project() {
    let s = load(SHOP, Dialect::Postgres).unwrap();
    let m = &s.manifest;
    let orders = &m.nodes["model.shop.orders"];
    assert_eq!(orders.materialized(), Some(Materialized::Table));
    assert_eq!(
        orders.relation.as_ref().unwrap().rendered,
        r#""analytics_marts"."orders""#
    );
    assert_eq!(
        orders.depends_on.iter().collect::<Vec<_>>(),
        ["model.shop.int_paid", "seed.shop.countries"]
    );
    let stg = &m.nodes["model.shop.stg_orders"];
    assert_eq!(
        stg.relation.as_ref().unwrap().rendered,
        r#""analytics"."stg_orders""#
    );
    assert_eq!(
        stg.sources.iter().next().unwrap(),
        &("raw".into(), "orders".into())
    );
    assert_eq!(
        m.nodes["model.shop.int_paid"].materialized(),
        Some(Materialized::Ephemeral)
    );
    assert_eq!(
        m.nodes["snapshot.shop.orders_snapshot"].resource_type,
        ResourceType::Snapshot
    );
    assert_eq!(
        m.nodes["snapshot.shop.orders_snapshot"].config.0["strategy"],
        "check"
    );

    let tests: Vec<&str> = m
        .nodes
        .values()
        .filter(|n| n.is_test())
        .map(|n| n.name.as_str())
        .collect();
    assert_eq!(
        tests,
        [
            "assert_no_negative",
            "not_null_orders_id",
            "not_null_source_raw_orders_id",
            "relationships_orders_country",
            "unique_orders_id",
        ]
    );
    let rel = &m.nodes["test.shop.relationships_orders_country"];
    assert!(
        rel.depends_on.contains("seed.shop.countries"),
        "{:?}",
        rel.depends_on
    );
    assert!(rel.depends_on.contains("model.shop.orders"));
    assert!(matches!(rel.kind, NodeKind::GenericTest { .. }));
    assert!(
        m.nodes["test.shop.assert_no_negative"]
            .depends_on
            .contains("model.shop.orders")
    );
}

#[test]
fn macros_override_schema_naming() {
    let mut files = SHOP.to_vec();
    files.push((
        "macros/naming.sql",
        "{% macro generate_schema_name(custom, node) %}{{ custom if custom else target.schema }}{% endmacro %}",
    ));
    let s = load(&files, Dialect::Postgres).unwrap();
    assert_eq!(
        s.manifest.nodes["model.shop.orders"]
            .relation
            .as_ref()
            .unwrap()
            .rendered,
        r#""marts"."orders""#
    );
}

#[test]
fn sqlite_rejects_custom_schemas() {
    let err = load(SHOP, Dialect::Sqlite).err().unwrap();
    assert!(err.0.contains("SQLite has no schemas"), "{err}");
}

#[test]
fn reports_cycles_unknown_refs_and_tests() {
    let err = load(
        &[("models/a.sql", "select * from {{ ref('nope') }}")],
        Dialect::Postgres,
    )
    .err()
    .unwrap();
    assert!(err.0.contains("not a model, seed or snapshot"), "{err}");

    let err = load(
        &[
            ("models/a.sql", "select 1"),
            (
                "models/s.yml",
                "models:\n  - name: a\n    columns:\n      - name: x\n        tests: [is_even]\n",
            ),
        ],
        Dialect::Postgres,
    )
    .err()
    .unwrap();
    assert!(err.0.contains("no generic test named \"is_even\""), "{err}");

    let err = load(
        &[("models/s.yml", "models:\n  - name: ghost\n")],
        Dialect::Postgres,
    )
    .err()
    .unwrap();
    assert!(err.0.contains("does not exist"), "{err}");
}

#[test]
fn disabled_nodes_are_dropped_and_cannot_be_referenced() {
    let s = load(
        &[
            ("models/a.sql", "{{ config(enabled=false) }} select 1"),
            ("models/b.sql", "select 2"),
        ],
        Dialect::Postgres,
    )
    .unwrap();
    assert!(!s.manifest.nodes.contains_key("model.dbtindb.a"));
    let err = load(
        &[
            ("models/a.sql", "{{ config(enabled=false) }} select 1"),
            ("models/b.sql", "select * from {{ ref('a') }}"),
        ],
        Dialect::Postgres,
    )
    .err()
    .unwrap();
    assert!(err.0.contains("disabled"), "{err}");
}
