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

#[test]
fn analyses_compile_but_are_not_referenceable() {
    let s = load(
        &[
            ("models/a.sql", "select 1 as id"),
            ("analyses/report.sql", "select count(*) from {{ ref('a') }}"),
        ],
        Dialect::Postgres,
    )
    .unwrap();
    let report = &s.manifest.nodes["analysis.dbtindb.report"];
    assert_eq!(report.resource_type, ResourceType::Analysis);
    assert!(report.relation.is_none());
    assert!(report.depends_on.contains("model.dbtindb.a"));
    assert_eq!(
        crate::compile(&s, "report").unwrap(),
        "select count(*) from \"analytics\".\"a\""
    );
    let err = load(
        &[
            ("analyses/report.sql", "select 1"),
            ("models/b.sql", "select * from {{ ref('report') }}"),
        ],
        Dialect::Postgres,
    )
    .err()
    .unwrap();
    assert!(err.0.contains("report"), "{err}");
}

#[test]
fn descriptions_render_docs_blocks() {
    let s = load(
        &[
            ("models/orders.sql", "select 1 as id"),
            (
                "models/orders.yml",
                "version: 2\nmodels:\n  - name: orders\n    description: \"{{ doc('orders') }}\"\n    \
                 columns:\n      - name: id\n        description: \"{{ doc('dbtindb', 'order_id') }}\"\n",
            ),
            (
                "models/docs.md",
                "{% docs orders %}\nOne row per {order}.\n{% enddocs %}\n{% docs order_id %}The key.{% enddocs %}",
            ),
        ],
        Dialect::Postgres,
    )
    .unwrap();
    let orders = &s.manifest.nodes["model.dbtindb.orders"];
    assert_eq!(orders.description, "One row per {order}.");
    assert_eq!(orders.columns[0].description, "The key.");
    let err = load(
        &[
            ("models/a.sql", "select 1"),
            (
                "models/a.yml",
                "version: 2\nmodels:\n  - name: a\n    description: \"{{ doc('nope') }}\"\n",
            ),
        ],
        Dialect::Postgres,
    )
    .err()
    .unwrap();
    assert!(err.0.contains("no docs block named \"nope\""), "{err}");
}

#[test]
fn tests_that_store_failures_get_audit_relations() {
    let files = [
        (
            "dbt_project.yml",
            "name: dbtindb\ndata_tests:\n  +store_failures: true\n",
        ),
        ("models/orders.sql", "select 1 as id"),
        (
            "models/orders.yml",
            "models:\n  - name: orders\n    columns:\n      - name: id\n        data_tests:\n          - unique\n          \
             - not_null:\n              config: {store_failures_as: ephemeral}\n      \
             - name: a_column_whose_name_goes_on_and_on_and_on_and_on\n        data_tests:\n          \
             - accepted_values:\n              values: [1]\n              config: {store_failures_as: view}\n",
        ),
    ];
    let s = load(&files, Dialect::Postgres).unwrap();
    let rel = |name: &str| {
        s.manifest
            .nodes
            .values()
            .find(|n| n.name.starts_with(name))
            .unwrap()
            .relation
            .as_ref()
            .map(|r| r.rendered.clone())
    };
    assert_eq!(
        rel("unique_").as_deref(),
        Some("\"analytics_dbt_test__audit\".\"unique_orders_id\"")
    );
    // accepted_values_orders_a_column_whose_name_goes_on_and_on_and_on_and_on
    // is too long for an identifier.
    assert_eq!(
        rel("accepted_values_").as_deref(),
        Some(
            "\"analytics_dbt_test__audit\".\"accepted_values_orders_a_colum_\
             43968c4bda22a788e4bb2b89d4ef1be8\""
        )
    );
    assert_eq!(rel("not_null_"), None);
    let s = load(&files, Dialect::Sqlite).unwrap();
    assert_eq!(
        s.manifest.nodes["test.dbtindb.unique_orders_id"]
            .relation
            .as_ref()
            .unwrap()
            .rendered,
        "\"unique_orders_id\""
    );
}

#[test]
fn yaml_snapshots_snapshot_a_ref_or_source() {
    let yml = |relation: &str| {
        format!(
            "snapshots:\n  - name: snap\n    relation: \"{relation}\"\n    \
             config: {{unique_key: id, strategy: check, check_cols: all}}\n"
        )
    };
    let ok = yml("ref('m')");
    let s = load(
        &[("models/m.sql", "select 1 as id"), ("snapshots/s.yml", &ok)],
        Dialect::Postgres,
    )
    .unwrap();
    let snap = &s.manifest.nodes["snapshot.dbtindb.snap"];
    assert!(snap.depends_on.contains("model.dbtindb.m"));
    assert_eq!(snap.path, "snapshots/s.yml");
    let bad = yml("analytics.m");
    let err = load(&[("snapshots/s.yml", &bad)], Dialect::Postgres)
        .err()
        .unwrap();
    assert!(err.0.contains("must be a ref() or source() call"), "{err}");
    let err = load(
        &[
            ("models/m.sql", "select 1 as id"),
            ("snapshots/s.yml", &ok),
            (
                "snapshots/snap.sql",
                "{% snapshot snap %}select 1 as id{% endsnapshot %}",
            ),
        ],
        Dialect::Postgres,
    )
    .err()
    .unwrap();
    assert!(
        err.0
            .contains("defined both here and in snapshots/snap.sql"),
        "{err}"
    );
}

#[test]
fn unit_tests_become_nodes_of_their_model() {
    let unit = "unit_tests:\n  - name: t\n    model: m\n    given: []\n    expect: {rows: []}\n";
    let s = load(
        &[("models/m.sql", "select 1 as id"), ("models/u.yml", unit)],
        Dialect::Postgres,
    )
    .unwrap();
    let t = &s.manifest.nodes["unit_test.dbtindb.m.t"];
    assert_eq!(t.resource_type, ResourceType::UnitTest);
    assert_eq!(t.depends_on.iter().collect::<Vec<_>>(), ["model.dbtindb.m"]);
    assert_eq!(t.fqn, ["dbtindb", "m", "t"]);

    let disabled = load(
        &[
            ("models/m.sql", "{{ config(enabled=false) }} select 1"),
            ("models/u.yml", unit),
        ],
        Dialect::Postgres,
    )
    .unwrap();
    assert!(disabled.manifest.nodes.is_empty());
    let err = load(&[("models/u.yml", unit)], Dialect::Postgres)
        .err()
        .unwrap();
    assert!(err.0.contains("tests \"m\", which is not a model"), "{err}");
}

#[test]
fn configs_that_would_change_the_build_are_errors() {
    for (config, msg) in [
        ("contract={'enforced': true}", "enforced model contracts"),
        ("incremental_predicates=['a > 1']", "incremental_predicates"),
    ] {
        let model = format!("{{{{ config({config}) }}}} select 1");
        let err = load(&[("models/m.sql", &model)], Dialect::Postgres)
            .err()
            .unwrap();
        assert!(err.0.contains(msg), "{err}");
    }
    // An unenforced contract changes nothing in dbt either.
    load(
        &[(
            "models/m.sql",
            "{{ config(contract={'enforced': false}) }} select 1",
        )],
        Dialect::Postgres,
    )
    .unwrap();
    let err = Project::new([
        ("models/m.sql".to_owned(), "select 1".to_owned()),
        (
            "models/s.yml".to_owned(),
            "models:\n  - name: m\n    versions: [{v: 1}]\n".to_owned(),
        ),
    ])
    .unwrap_err();
    assert!(err.0.contains("model versions"), "{err}");
}
