//! Packages and dbt's own macros: namespaces, scoping and dispatch.

use crate::parse_tests::load;
use crate::{Dialect, NodeKind, Session};

/// A small package in the shape of dbt_utils: a dispatching macro, a
/// package-internal helper called unqualified, a statement block and a
/// generic test.
const PKG: &[(&str, &str)] = &[
    (
        "dbt_packages/tools/dbt_project.yml",
        "name: tools\nmacro-paths: [macros]\nvars:\n  tools_default: 7\n",
    ),
    (
        "dbt_packages/tools/macros/greet.sql",
        "{% macro greet(x) %}{{ return(adapter.dispatch('greet', 'tools')(x)) }}{% endmacro %}\n\
         {% macro default__greet(x) %}hello {{ helper(x) }}{% endmacro %}",
    ),
    (
        "dbt_packages/tools/macros/statements.sql",
        "{% macro st() %}{% call statement('a', fetch_result=True) %}select 1{% endcall %}{% endmacro %}",
    ),
    (
        "dbt_packages/tools/macros/helper.sql",
        "{% macro helper(x) %}<{{ x }}>{% endmacro %}",
    ),
    (
        "dbt_packages/tools/macros/tests.sql",
        "{% test positive(model, column_name) %}select * from {{ model }} where {{ column_name }} <= 0{% endtest %}",
    ),
];

fn session(extra: &[(&str, &str)], dialect: Dialect) -> Session {
    let files: Vec<_> = PKG.iter().chain(extra).copied().collect();
    load(&files, dialect).unwrap()
}

fn inline(s: &Session, sql: &str) -> String {
    s.compile_inline(sql).unwrap()
}

#[test]
fn calls_package_macros_by_namespace() {
    let s = session(&[("models/m.sql", "select 1")], Dialect::Postgres);
    assert_eq!(inline(&s, "{{ tools.greet('a') }}"), "hello <a>");
    // The package's vars are defaults.
    assert_eq!(inline(&s, "{{ var('tools_default') }}"), "7");
}

#[test]
fn package_macros_resolve_in_their_package_first() {
    // The root project's own `helper` does not replace the package's
    // internal one, as in dbt.
    let s = session(
        &[(
            "macros/helper.sql",
            "{% macro helper(x) %}ROOT{% endmacro %}",
        )],
        Dialect::Postgres,
    );
    assert_eq!(inline(&s, "{{ tools.greet('a') }}"), "hello <a>");
    assert_eq!(inline(&s, "{{ helper('a') }}"), "ROOT");
}

#[test]
fn the_root_project_overrides_dispatched_implementations() {
    let s = session(
        &[(
            "macros/greet.sql",
            "{% macro postgres__greet(x) %}hi {{ x }}{% endmacro %}",
        )],
        Dialect::Postgres,
    );
    assert_eq!(inline(&s, "{{ tools.greet('a') }}"), "hi a");
    // On another adapter, the package's default still applies.
    let s = session(
        &[(
            "macros/greet.sql",
            "{% macro postgres__greet(x) %}hi {{ x }}{% endmacro %}",
        )],
        Dialect::Sqlite,
    );
    assert_eq!(inline(&s, "{{ tools.greet('a') }}"), "hello <a>");
}

#[test]
fn dispatch_config_sets_the_search_order() {
    let s = session(
        &[
            (
                "dbt_project.yml",
                "name: shop\ndispatch:\n  - macro_namespace: tools\n    search_order: [tools]\n",
            ),
            (
                "macros/greet.sql",
                "{% macro default__greet(x) %}root{% endmacro %}",
            ),
        ],
        Dialect::Postgres,
    );
    assert_eq!(inline(&s, "{{ tools.greet('a') }}"), "hello <a>");
}

#[test]
fn package_generic_tests_are_namespaced() {
    let s = session(
        &[
            ("models/m.sql", "select 1 as id"),
            (
                "models/m.yml",
                "models:\n  - name: m\n    columns:\n      - name: id\n        data_tests: [tools.positive]\n",
            ),
        ],
        Dialect::Postgres,
    );
    let test = s
        .manifest
        .nodes
        .values()
        .find(|n| matches!(n.kind, NodeKind::GenericTest { .. }))
        .unwrap();
    assert_eq!(test.name, "tools_positive_m_id");
    assert_eq!(
        s.compile(&test.unique_id, false).unwrap(),
        "select * from \"analytics\".\"m\" where id <= 0"
    );
}

#[test]
fn renders_dbt_cross_database_macros() {
    let pg = session(&[], Dialect::Postgres);
    assert_eq!(inline(&pg, "{{ dbt.type_string() }}"), "TEXT");
    assert_eq!(inline(&pg, "{{ type_numeric() }}"), "numeric(28,6)");
    assert_eq!(
        inline(&pg, "{{ dbt.split_part('s', \"','\", 2) }}").trim(),
        "split_part(\n        s,\n        ',',\n        2\n        )"
    );
    assert_eq!(inline(&pg, "{{ dbt.current_timestamp() }}"), "now()");
    assert!(inline(&pg, "{{ dbt.dateadd('day', 1, 'd') }}").contains("interval"));
    assert_eq!(inline(&pg, "{{ dbt.concat(['a', 'b']) }}"), "a || b");

    let lite = session(&[], Dialect::Sqlite);
    assert_eq!(inline(&lite, "{{ dbt.type_timestamp() }}"), "TEXT");
    assert_eq!(
        inline(&lite, "{{ dbt.split_part('s', \"','\", 2) }}"),
        "dbtindb_split_part(s, ',', 2)"
    );
    assert_eq!(
        inline(&lite, "{{ dbt.date_trunc('month', 'd') }}"),
        "date(d, 'start of month')"
    );
    assert_eq!(
        inline(&lite, "{{ dbt.last_day('d', 'month') }}"),
        "date(date(d, 'start of month'), '+1 month', '-1 day')"
    );
    assert_eq!(inline(&lite, "{{ dbt.date(2024, 2, 9) }}"), "'2024-02-09'");
    let err = lite
        .compile_inline("{{ dbt.array_construct([1]) }}")
        .unwrap_err();
    assert!(err.0.contains("no array type"), "{err}");
}

#[test]
fn provides_python_helpers() {
    let s = session(&[], Dialect::Postgres);
    assert_eq!(
        inline(&s, "{{ set(['b', 'a', 'b']) == set(['a', 'b']) }}"),
        "True"
    );
    assert_eq!(
        inline(&s, "{{ zip([1, 2], ['a', 'b', 'c']) | list }}"),
        "[(1, 'a'), (2, 'b')]"
    );
    assert_eq!(inline(&s, "{{ fromyaml('a: [1, 2]').a[1] }}"), "2");
    assert_eq!(
        inline(&s, "{{ modules.re.sub('[0-9]', '#', 'a1b2') }}"),
        "a#b#"
    );
    assert_eq!(inline(&s, "{{ flags.WHICH }}"), "compile");
    assert_eq!(inline(&s, "{{ run_started_at.year > 2000 }}"), "True");
    assert_eq!(
        inline(&s, "{{ api.Relation.create(schema='s', identifier='t') }}"),
        "\"s\".\"t\""
    );
}

#[test]
fn rejects_unknown_packages_and_dotted_dispatch() {
    let s = session(&[], Dialect::Postgres);
    let err = s
        .compile_inline("{{ adapter.dispatch('tools.greet')() }}")
        .unwrap_err();
    assert!(err.0.contains("macro_namespace"), "{err}");
    let err = s
        .compile_inline("{{ adapter.dispatch('nope', 'tools')() }}")
        .unwrap_err();
    assert!(err.0.contains("tools.default__nope"), "{err}");
}

#[test]
fn statement_blocks_run_their_body_where_it_was_written() {
    // The body calls a macro of its own file, which only resolves in the
    // template state the body was written in. Without a database, the
    // statement then fails at running the SQL, which shows the body ran.
    let s = session(
        &[(
            "macros/outer.sql",
            "{% macro inner() %}select 1{% endmacro %}\n\
             {% macro outer() %}{% call statement('a', fetch_result=True) %}{{ inner() }}{% endcall %}{% endmacro %}",
        )],
        Dialect::Postgres,
    );
    for sql in [
        "{{ outer() }}",
        "{{ tools.st() }}",
        "{{ dbt.get_columns_in_query('select 1') }}",
    ] {
        let err = s.compile_inline(sql).unwrap_err();
        assert!(err.0.contains("no database connection"), "{sql}: {err}");
    }
}
