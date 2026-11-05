//! `{% docs name %}...{% enddocs %}` blocks in `.md` files, which
//! descriptions reference with `{{ doc('name') }}`.

use std::collections::BTreeMap;

use crate::Error;

/// Adds the docs blocks of the file at `path` to `docs`, by name.
///
/// A block's text is what lies between its tags, trimmed, as in dbt. It is
/// not rendered: Markdown is full of braces Jinja would misread.
pub fn add_file(docs: &mut BTreeMap<String, String>, path: &str, text: &str) -> Result<(), Error> {
    let mut rest = text;
    while let Some((tag, after)) = next_tag(rest) {
        let mut words = tag.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some("docs"), Some(name), None) => {
                let (body, after_end) = find_end(after).ok_or_else(|| {
                    Error::new(format!(
                        "{path}: docs block {name:?} has no {{% enddocs %}}"
                    ))
                })?;
                if docs
                    .insert(name.to_owned(), body.trim().to_owned())
                    .is_some()
                {
                    return Err(Error::new(format!(
                        "{path}: there are two docs blocks named {name:?}"
                    )));
                }
                rest = after_end;
            }
            (Some("docs"), _, _) => {
                return Err(Error::new(format!(
                    "{path}: a docs block takes one name: {{% docs <name> %}}"
                )));
            }
            _ => rest = after,
        }
    }
    Ok(())
}

/// The next `{% ... %}` tag: its inside without whitespace-control dashes,
/// and the text after it.
fn next_tag(text: &str) -> Option<(&str, &str)> {
    let start = text.find("{%")?;
    let inner = &text[start + 2..];
    let end = inner.find("%}")?;
    let tag = inner[..end].trim_start_matches('-').trim_end_matches('-');
    Some((tag.trim(), &inner[end + 2..]))
}

/// The body up to the next `{% enddocs %}`, and the text after it.
fn find_end(text: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    loop {
        let start = offset + text[offset..].find("{%")?;
        let (tag, after) = next_tag(&text[start..])?;
        if tag == "enddocs" {
            return Some((&text[..start], after));
        }
        offset = text.len() - after.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_blocks_and_trims_them() {
        let mut docs = BTreeMap::new();
        add_file(
            &mut docs,
            "models/docs.md",
            "# Notes\n{% docs orders %}\nOne row per **order**: `{id}`.\n{% enddocs %}\n\
             {%- docs status -%}Paid or open.{%- enddocs -%}",
        )
        .unwrap();
        assert_eq!(docs["orders"], "One row per **order**: `{id}`.");
        assert_eq!(docs["status"], "Paid or open.");
    }

    #[test]
    fn rejects_duplicates_and_unclosed_blocks() {
        let mut docs = BTreeMap::new();
        let err = add_file(
            &mut docs,
            "a.md",
            "{% docs x %}a{% enddocs %}{% docs x %}b{% enddocs %}",
        )
        .unwrap_err();
        assert!(err.0.contains("two docs blocks"), "{err}");
        let err = add_file(&mut BTreeMap::new(), "a.md", "{% docs x %}a").unwrap_err();
        assert!(err.0.contains("no {% enddocs %}"), "{err}");
    }
}
