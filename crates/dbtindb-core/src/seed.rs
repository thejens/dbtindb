//! Seeds: CSV files loaded into tables, with column types inferred from
//! the values as dbt does, or set with the `column_types` config.

use crate::config::Config;
use crate::{Dialect, Error, SeedType};

/// A parsed seed: typed columns and rows of text (`None` for empty cells).
#[derive(Debug, PartialEq)]
pub struct Seed {
    pub columns: Vec<(String, String)>,
    pub rows: Vec<Vec<Option<String>>>,
}

pub fn parse(path: &str, text: &str, dialect: Dialect, config: &Config) -> Result<Seed, Error> {
    let delimiter = match config.str("delimiter")? {
        None => b',',
        Some(d) if d.len() == 1 => d.as_bytes()[0],
        Some(d) => {
            return Err(Error::new(format!(
                "{path}: delimiter must be one character, not {d:?}"
            )));
        }
    };
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .from_reader(text.as_bytes());
    let headers: Vec<String> = reader
        .headers()
        .map_err(|e| Error::new(format!("{path}: {e}")))?
        .iter()
        .map(|h| h.trim().to_owned())
        .collect();
    if headers.is_empty() || headers.iter().any(String::is_empty) {
        return Err(Error::new(format!(
            "{path}: every column needs a name in the header row"
        )));
    }
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|e| Error::new(format!("{path}: {e}")))?;
        rows.push(
            record
                .iter()
                .map(|v| (!v.is_empty()).then(|| v.to_owned()))
                .collect::<Vec<_>>(),
        );
    }
    let overrides = config
        .get("column_types")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let mut columns = Vec::with_capacity(headers.len());
    for (i, name) in headers.iter().enumerate() {
        let ty = match overrides.get(name).and_then(|v| v.as_str()) {
            Some(t) => t.to_owned(),
            None => {
                let kind = infer(rows.iter().filter_map(|r| r[i].as_deref()));
                // SQLite has no boolean type; store 1 and 0 as dbt-sqlite does.
                if kind == SeedType::Boolean && dialect == Dialect::Sqlite {
                    for r in &mut rows {
                        if let Some(v) = &mut r[i] {
                            *v = if v.eq_ignore_ascii_case("true") {
                                "1"
                            } else {
                                "0"
                            }
                            .to_owned();
                        }
                    }
                }
                dialect.seed_type(kind).to_owned()
            }
        };
        columns.push((name.clone(), ty));
    }
    Ok(Seed { columns, rows })
}

/// The narrowest kind every value fits. A column with no values is text.
fn infer<'a>(values: impl Iterator<Item = &'a str> + Clone) -> SeedType {
    let all = |f: fn(&str) -> bool| {
        let mut any = false;
        for v in values.clone() {
            if !f(v) {
                return false;
            }
            any = true;
        }
        any
    };
    if all(is_integer) {
        SeedType::Integer
    } else if all(is_number) {
        SeedType::Number
    } else if all(|v| v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false")) {
        SeedType::Boolean
    } else if all(is_date) {
        SeedType::Date
    } else if all(is_timestamp) {
        SeedType::Timestamp
    } else {
        SeedType::Text
    }
}

fn is_integer(v: &str) -> bool {
    // Leading zeros (zip codes, ids like 007) are text, as in dbt.
    let digits = v.strip_prefix('-').unwrap_or(v);
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'))
        && v.parse::<i64>().is_ok()
}

fn is_number(v: &str) -> bool {
    let body = v.strip_prefix('-').unwrap_or(v);
    let mut parts = body.splitn(2, '.');
    let int = parts.next().unwrap_or("");
    let frac = parts.next();
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    !body.is_empty()
        && digits(int)
        && frac.is_none_or(|f| !f.is_empty() && digits(f))
        && !(int.is_empty() && frac.is_none())
        && (int.len() <= 1 || !int.starts_with('0'))
}

fn is_date(v: &str) -> bool {
    let b = v.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && [0, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|&i| b[i].is_ascii_digit())
        && (1..=12).contains(&v[5..7].parse::<u32>().unwrap_or(0))
        && (1..=31).contains(&v[8..10].parse::<u32>().unwrap_or(0))
}

fn is_timestamp(v: &str) -> bool {
    if v.len() < 19 || !is_date(&v[..10]) || !matches!(v.as_bytes()[10], b' ' | b'T') {
        return false;
    }
    let t = &v[11..];
    let b = t.as_bytes();
    b.len() >= 8
        && b[2] == b':'
        && b[5] == b':'
        && [0, 1, 3, 4, 6, 7].iter().all(|&i| b[i].is_ascii_digit())
        && t[8..].strip_prefix('.').map_or(t.len() == 8, |f| {
            !f.is_empty() && f.bytes().all(|c| c.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn infers_types_like_dbt() {
        let csv = "id,price,active,day,at,zip,note\n1,9.5,true,2024-01-31,2024-01-31 10:00:00,0123,hi\n2,10,False,2024-02-01,2024-02-01T11:30:00.5,4567,\n";
        let seed = parse("seeds/x.csv", csv, Dialect::Postgres, &Config::default()).unwrap();
        let types: Vec<&str> = seed.columns.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(
            types,
            [
                "bigint",
                "numeric",
                "boolean",
                "date",
                "timestamp",
                "text",
                "text"
            ]
        );
        assert_eq!(seed.rows[1][6], None);

        let seed = parse("seeds/x.csv", csv, Dialect::Sqlite, &Config::default()).unwrap();
        assert_eq!(seed.columns[2].1, "INTEGER");
        assert_eq!(seed.rows[1][2].as_deref(), Some("0"));
    }

    #[test]
    fn honours_column_types_and_delimiter() {
        let mut config = Config::default();
        config.merge(
            json!({"column_types": {"id": "varchar(10)"}, "delimiter": ";"})
                .as_object()
                .unwrap(),
        );
        let seed = parse("seeds/x.csv", "id;n\n1;2\n", Dialect::Postgres, &config).unwrap();
        assert_eq!(
            seed.columns,
            [
                ("id".into(), "varchar(10)".into()),
                ("n".into(), "bigint".into())
            ]
        );
    }
}
