//! Running data tests: counting failing rows and applying `severity`,
//! `warn_if` and `error_if`.

use serde_json::Value;

use crate::Error;
use crate::node::Node;

use super::Runner;

pub struct Outcome {
    pub status: String,
    pub failures: i64,
    pub sql: String,
}

pub fn run(r: &Runner<'_>, node: &Node) -> Result<Outcome, Error> {
    let sql = r.s.compile(&node.unique_id, false)?;
    let count = format!("SELECT count(*) AS failures FROM (\n{sql}\n) dbt_internal_test");
    let table =
        r.db.query(&count)
            .map_err(|e| Error::new(format!("{e}\nwhile running test {}:\n{count}", node.name)))?;
    let failures = match table.rows.first().and_then(|row| row.first()) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    };
    let c = &node.config;
    let severity = c.str("severity")?.unwrap_or("error").to_ascii_lowercase();
    let error_if = threshold(c.str("error_if")?.unwrap_or("!= 0"))?;
    let warn_if = threshold(c.str("warn_if")?.unwrap_or("!= 0"))?;
    let status = match severity.as_str() {
        "error" if error_if(failures) => "fail",
        "error" | "warn" if warn_if(failures) => "warn",
        "error" | "warn" => "pass",
        other => {
            return Err(Error::new(format!(
                "test {}: severity {other:?} is not error or warn",
                node.name
            )));
        }
    };
    Ok(Outcome {
        status: status.into(),
        failures,
        sql: count,
    })
}

/// Parses a condition such as `!= 0`, `>10` or `>= 5`.
fn threshold(cond: &str) -> Result<Box<dyn Fn(i64) -> bool>, Error> {
    let cond = cond.trim();
    let ops = ["!=", "<>", ">=", "<=", "==", "=", ">", "<"];
    let op = ops
        .iter()
        .find(|op| cond.starts_with(*op))
        .ok_or_else(|| Error::new(format!("invalid test threshold {cond:?}")))?;
    let n: i64 = cond[op.len()..]
        .trim()
        .parse()
        .map_err(|_| Error::new(format!("invalid test threshold {cond:?}")))?;
    Ok(match *op {
        "!=" | "<>" => Box::new(move |x| x != n),
        ">=" => Box::new(move |x| x >= n),
        "<=" => Box::new(move |x| x <= n),
        "==" | "=" => Box::new(move |x| x == n),
        ">" => Box::new(move |x| x > n),
        _ => Box::new(move |x| x < n),
    })
}
