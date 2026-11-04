//! dbt's own cross-database macros (`dbt.dateadd`, `dbt.type_string`,
//! `statement`, ...), vendored from dbt-adapters and dbt-postgres
//! (Apache-2.0; see `dbt-macros/NOTICE`), plus dbtindb's SQLite
//! implementations of them.

/// `(path, source)` of each file, embedded at build time.
macro_rules! files {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_str!(concat!("../../dbt-macros/", $path)))),*]
    };
}

/// Every adapter's: the dispatching macros and their `default__` versions,
/// and dbtindb's versions of the adapter macros it implements natively.
pub const COMMON: &[(&str, &str)] = files![
    "dbt-adapters/macros/utils/any_value.sql",
    "dbt-adapters/macros/utils/array_append.sql",
    "dbt-adapters/macros/utils/array_concat.sql",
    "dbt-adapters/macros/utils/array_construct.sql",
    "dbt-adapters/macros/utils/bool_or.sql",
    "dbt-adapters/macros/utils/cast.sql",
    "dbt-adapters/macros/utils/cast_bool_to_text.sql",
    "dbt-adapters/macros/utils/concat.sql",
    "dbt-adapters/macros/utils/data_types.sql",
    "dbt-adapters/macros/utils/date.sql",
    "dbt-adapters/macros/utils/date_spine.sql",
    "dbt-adapters/macros/utils/date_trunc.sql",
    "dbt-adapters/macros/utils/dateadd.sql",
    "dbt-adapters/macros/utils/datediff.sql",
    "dbt-adapters/macros/utils/equals.sql",
    "dbt-adapters/macros/utils/escape_single_quotes.sql",
    "dbt-adapters/macros/utils/except.sql",
    "dbt-adapters/macros/utils/generate_series.sql",
    "dbt-adapters/macros/utils/hash.sql",
    "dbt-adapters/macros/utils/intersect.sql",
    "dbt-adapters/macros/utils/last_day.sql",
    "dbt-adapters/macros/utils/length.sql",
    "dbt-adapters/macros/utils/listagg.sql",
    "dbt-adapters/macros/utils/literal.sql",
    "dbt-adapters/macros/utils/position.sql",
    "dbt-adapters/macros/utils/replace.sql",
    "dbt-adapters/macros/utils/right.sql",
    "dbt-adapters/macros/utils/safe_cast.sql",
    "dbt-adapters/macros/utils/split_part.sql",
    "dbt-adapters/macros/etc/datetime.sql",
    "dbt-adapters/macros/adapters/timestamps.sql",
    "dbt-adapters/macros/materializations/configs.sql",
    "dbt-adapters/macros/materializations/tests/where_subquery.sql",
    "dbtindb/adapters.sql",
];

/// dbt-postgres's `postgres__` versions.
pub const POSTGRES: &[(&str, &str)] = files![
    "dbt-postgres/macros/utils/any_value.sql",
    "dbt-postgres/macros/utils/dateadd.sql",
    "dbt-postgres/macros/utils/datediff.sql",
    "dbt-postgres/macros/utils/last_day.sql",
    "dbt-postgres/macros/utils/listagg.sql",
    "dbt-postgres/macros/utils/split_part.sql",
    "dbt-postgres/macros/timestamps.sql",
];

/// dbtindb's `sqlite__` versions.
pub const SQLITE: &[(&str, &str)] = files!["sqlite/utils.sql",];

/// The files for one dialect.
pub fn files(
    dialect: crate::Dialect,
) -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    let own = match dialect {
        crate::Dialect::Postgres => POSTGRES,
        crate::Dialect::Sqlite => SQLITE,
    };
    COMMON.iter().chain(own)
}
