//! Running unit tests: the model's output on fixture rows, compared with
//! the expected rows as multisets over the columns `expect` names, as dbt
//! does. Every row in one result and not the other is a failure.

use crate::node::{Node, NodeKind};
use crate::unit_test::{fixture_sql, row_columns};
use crate::{Column, Error};

use super::Runner;
use super::materialize::run;
use super::test::Outcome;

/// How many differing rows a failure shows.
const SHOWN: usize = 20;

pub fn run_unit_test(r: &Runner<'_>, node: &Node) -> Result<Outcome, Error> {
    let NodeKind::UnitTest(def) = &node.kind else {
        unreachable!("only unit tests run as unit tests");
    };
    let d = r.db.dialect();
    let err = |what: String| Error::new(format!("unit test {}: {what}", node.name));
    let sql = r.s.compile(&node.unique_id, false)?;
    let mut stmts = Vec::new();

    // Fixed names: unit tests run one at a time, and a name built from the
    // test's could pass PostgreSQL's identifier limit.
    let actual = "dbt_unit_test_actual";
    stmts.push(run(r, d.create_temp_table_as(actual, &sql))?);
    let actual_cols = r.db.columns(d.temp_schema(), actual)?;
    let expected = "dbt_unit_test_expected";
    let compared: Vec<Column> = match row_columns(d, &def.expect)? {
        // Typed as the model's output, so values compare as they would in
        // the model's table.
        Some(names) => names
            .iter()
            .map(|n| {
                actual_cols
                    .iter()
                    .find(|c| &c.name == n)
                    .cloned()
                    .ok_or_else(|| {
                        err(format!(
                            "expect has column {n:?}, which the model does not return"
                        ))
                    })
            })
            .collect::<Result<_, _>>()?,
        None => Vec::new(),
    };
    let expected_sql = fixture_sql(d, &def.expect, &compared, "expect").map_err(err)?;
    stmts.push(run(r, d.create_temp_table_as(expected, &expected_sql))?);
    let compared = if compared.is_empty() {
        r.db.columns(d.temp_schema(), expected)?
    } else {
        compared
    };

    let (actual_q, expected_q) = (d.quote(actual), d.quote(expected));
    let diff = if compared.is_empty() {
        // `expect: {rows: []}` names no columns: only the row counts compare.
        format!(
            "SELECT 'actual' AS dbt_side, n AS dbt_rows FROM (SELECT count(*) AS n FROM {actual_q}) a \
             WHERE n <> (SELECT count(*) FROM {expected_q})"
        )
    } else {
        let list = compared
            .iter()
            .map(|c| d.quote(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        // Grouping treats NULLs as equal, which a plain EXCEPT would too,
        // and the counts keep duplicate rows apart.
        format!(
            "SELECT {list}, CASE WHEN sum(dbt_a) > sum(dbt_e) THEN 'actual' ELSE 'expected' END AS dbt_side, \
             abs(sum(dbt_a) - sum(dbt_e)) AS dbt_rows\nFROM (\n\
             SELECT {list}, 1 AS dbt_a, 0 AS dbt_e FROM {actual_q}\nUNION ALL\n\
             SELECT {list}, 0, 1 FROM {expected_q}\n) dbt_internal_unit_test\n\
             GROUP BY {list}\nHAVING sum(dbt_a) <> sum(dbt_e)"
        )
    };
    let table =
        r.db.query(&diff)
            .map_err(|e| err(format!("{e}\nwhile comparing:\n{diff}")))?;
    let failures: i64 = table
        .rows
        .iter()
        .filter_map(|row| row.last())
        .map(|v| match v {
            serde_json::Value::Number(n) => n.as_i64().unwrap_or(0),
            serde_json::Value::String(s) => s.parse().unwrap_or(0),
            _ => 0,
        })
        .sum();
    stmts.push(diff);
    stmts.push(run(r, format!("DROP TABLE {actual_q}"))?);
    stmts.push(run(r, format!("DROP TABLE {expected_q}"))?);

    // The differing rows, as dbt shows them: which side has them, and how
    // many more times.
    let mut shown = String::new();
    for row in table.rows.iter().take(SHOWN) {
        let cells: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        shown.push_str(&format!("\n  {}", cells.join(" | ")));
    }
    if table.rows.len() > SHOWN {
        shown.push_str(&format!("\n  ... {} more", table.rows.len() - SHOWN));
    }
    if failures > 0 {
        let header: Vec<&str> = compared
            .iter()
            .map(|c| c.name.as_str())
            .chain(["side", "count"])
            .collect();
        r.db.log(&format!(
            "unit test {} failed; rows only in one result ({}):{shown}",
            node.name,
            header.join(" | ")
        ));
    }
    Ok(Outcome {
        status: if failures > 0 { "fail" } else { "pass" }.into(),
        failures,
        sql: stmts.join(";\n"),
    })
}
