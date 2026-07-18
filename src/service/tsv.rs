//! Generic `#`-header TSV parsing skeleton, shared by every batch
//! write target (`fact set` here; `metric add` / `map set` in
//! story-03).
//!
//! A batch is one `#col1\tcol2\t…` header line followed by data rows.
//! The header decides which columns are present; a row type reads the
//! columns it cares about by name and fills defaults for anything the
//! header omitted. Each data row is validated independently so callers
//! can either roll back the whole batch (atomic) or skip bad rows.

/// A row type constructible from one TSV data line, given the parsed
/// header. Implementors read named columns via [`col`] and apply their
/// own defaults / controlled-vocabulary checks, returning a
/// human-readable reason string on failure.
pub trait FromTsvRow: Sized {
    fn from_fields(header: &[String], fields: &[&str]) -> Result<Self, String>;
}

/// Look up column `name`'s value in one data row. `None` when the
/// header did not declare the column (→ the row type uses its
/// default) or the row is short. Trims surrounding whitespace.
pub fn col<'a>(header: &[String], fields: &[&'a str], name: &str) -> Option<&'a str> {
    let idx = header.iter().position(|h| h == name)?;
    fields.get(idx).map(|s| s.trim())
}

/// Parse a `#`-header TSV blob into rows, collecting per-row failures
/// as `(1-based data-row number, reason)`. A missing / malformed
/// header yields no rows and a single `(0, …)` error. Blank lines are
/// skipped and do not consume a row number.
pub fn parse_tsv<R: FromTsvRow>(input: &str) -> (Vec<R>, Vec<(usize, String)>) {
    let mut lines = input.lines();
    let header_line = loop {
        match lines.next() {
            Some(l) if l.trim().is_empty() => continue,
            Some(l) => break l,
            None => return (Vec::new(), vec![(0, "empty input: no #header line".into())]),
        }
    };
    let Some(header_body) = header_line.trim_start().strip_prefix('#') else {
        return (
            Vec::new(),
            vec![(0, format!("first line must be a #header, got {header_line:?}"))],
        );
    };
    let header: Vec<String> = header_body.split('\t').map(|s| s.trim().to_string()).collect();

    let mut rows = Vec::new();
    let mut errors = Vec::new();
    let mut n = 0usize;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        n += 1;
        let fields: Vec<&str> = line.split('\t').collect();
        match R::from_fields(&header, &fields) {
            Ok(r) => rows.push(r),
            Err(why) => errors.push((n, why)),
        }
    }
    (rows, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Kv {
        k: String,
        v: i64,
        tag: String,
    }
    impl FromTsvRow for Kv {
        fn from_fields(header: &[String], fields: &[&str]) -> Result<Self, String> {
            let k = col(header, fields, "k").ok_or("missing k")?.to_string();
            let v = col(header, fields, "v")
                .ok_or("missing v")?
                .parse::<i64>()
                .map_err(|e| format!("bad v: {e}"))?;
            // Optional column with a default.
            let tag = col(header, fields, "tag").unwrap_or("none").to_string();
            Ok(Kv { k, v, tag })
        }
    }

    #[test]
    fn parses_rows_and_applies_defaults() {
        let (rows, errs) = parse_tsv::<Kv>("#k\tv\na\t1\nb\t2\n");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            rows,
            vec![
                Kv { k: "a".into(), v: 1, tag: "none".into() },
                Kv { k: "b".into(), v: 2, tag: "none".into() },
            ]
        );
    }

    #[test]
    fn header_column_order_is_respected() {
        // tag before k — from_fields reads by name, not position.
        let (rows, _e) = parse_tsv::<Kv>("#tag\tk\tv\nx\ta\t1\n");
        assert_eq!(rows[0], Kv { k: "a".into(), v: 1, tag: "x".into() });
    }

    #[test]
    fn bad_row_is_collected_with_line_number() {
        let (rows, errs) = parse_tsv::<Kv>("#k\tv\na\t1\nb\tNaN\nc\t3\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].0, 2, "second data row is bad");
    }

    #[test]
    fn missing_header_is_a_line_zero_error() {
        let (rows, errs) = parse_tsv::<Kv>("a\t1\n");
        assert!(rows.is_empty());
        assert_eq!(errs[0].0, 0);
    }

    #[test]
    fn blank_lines_do_not_consume_row_numbers() {
        let (rows, errs) = parse_tsv::<Kv>("#k\tv\n\na\t1\n\nb\tNaN\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(errs[0].0, 2, "blank lines skipped; NaN is data row 2");
    }
}
