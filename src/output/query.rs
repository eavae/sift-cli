//! Renderer for `sift sql` / `sift _sql` result sets — a
//! runtime-resolved `(columns, rows-of-strings)` shape (unlike the
//! static-schema `RenderRow` path). TSV routes through the canonical
//! [`render_tabular`] emitter; table and JSON are handled here because
//! the column set is only known at runtime.

use std::io::Write;

use serde_json::{Map, Value};
use unicode_width::UnicodeWidthStr;

use crate::error::SiftError;
use crate::output::io_err;
use crate::output::tabular::{render_tabular, TabularView};
use crate::output::Format;

/// Borrowed view over a query result so it can flow through
/// [`render_tabular`] for the TSV path.
struct QueryView<'a> {
    columns: &'a [String],
    rows: &'a [Vec<String>],
}

impl TabularView for QueryView<'_> {
    fn columns(&self) -> Vec<&str> {
        self.columns.iter().map(String::as_str).collect()
    }
    fn rows(&self) -> Vec<Vec<String>> {
        self.rows.to_vec()
    }
}

/// Render a dynamic result set in the requested format.
///
/// - `Tsv` → canonical `#col\t…` via `render_tabular`.
/// - `Table` → aligned columns (same convention as `output::render`).
/// - `Json` → NDJSON, one object per row; every cell is a JSON string
///   (matches the "everything to text" stringification decision — an
///   empty string stands in for SQL NULL).
pub fn render_query<W: Write>(
    out: &mut W,
    fmt: Format,
    columns: &[String],
    rows: &[Vec<String>],
) -> Result<(), SiftError> {
    match fmt {
        Format::Tsv => render_tabular(out, &QueryView { columns, rows }),
        Format::Table => render_table(out, columns, rows),
        Format::Json => render_ndjson(out, columns, rows),
    }
}

fn render_table<W: Write>(
    out: &mut W,
    columns: &[String],
    rows: &[Vec<String>],
) -> Result<(), SiftError> {
    if columns.is_empty() {
        return Ok(());
    }
    let ncols = columns.len();
    let mut widths: Vec<usize> = columns.iter().map(|c| UnicodeWidthStr::width(c.as_str())).collect();
    for row in rows {
        for (i, c) in row.iter().enumerate().take(ncols) {
            let w = UnicodeWidthStr::width(c.as_str());
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }
    write_row(out, &widths, columns.iter().map(String::as_str))?;
    for row in rows {
        write_row(out, &widths, row.iter().map(String::as_str))?;
    }
    Ok(())
}

fn write_row<'a, W, I>(out: &mut W, widths: &[usize], cells: I) -> Result<(), SiftError>
where
    W: Write,
    I: Iterator<Item = &'a str>,
{
    let cells: Vec<&str> = cells.collect();
    let last = cells.len().saturating_sub(1);
    // Assemble into a String so we can trim trailing padding — a
    // NULL/empty final cell would otherwise leave dangling spaces.
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        line.push_str(cell);
        if i != last {
            let pad = widths
                .get(i)
                .copied()
                .unwrap_or(0)
                .saturating_sub(UnicodeWidthStr::width(*cell));
            for _ in 0..pad {
                line.push(' ');
            }
            line.push_str("  ");
        }
    }
    out.write_all(line.trim_end().as_bytes()).map_err(io_err)?;
    out.write_all(b"\n").map_err(io_err)?;
    Ok(())
}

fn render_ndjson<W: Write>(
    out: &mut W,
    columns: &[String],
    rows: &[Vec<String>],
) -> Result<(), SiftError> {
    for row in rows {
        let mut obj = Map::with_capacity(columns.len());
        for (i, col) in columns.iter().enumerate() {
            let cell = row.get(i).cloned().unwrap_or_default();
            obj.insert(col.clone(), Value::String(cell));
        }
        serde_json::to_writer(&mut *out, &Value::Object(obj))
            .map_err(|e| SiftError::Internal(format!("ndjson serialize: {e}")))?;
        out.write_all(b"\n").map_err(io_err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<String> {
        vec!["symbol".into(), "value".into()]
    }
    fn rows() -> Vec<Vec<String>> {
        vec![
            vec!["600519.CN-A".into(), "1.5".into()],
            vec!["000001.CN-A".into(), "".into()],
        ]
    }

    fn to_string(fmt: Format) -> String {
        let mut buf = Vec::new();
        render_query(&mut buf, fmt, &cols(), &rows()).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn tsv_uses_hash_header() {
        let s = to_string(Format::Tsv);
        assert_eq!(s.lines().next().unwrap(), "#symbol\tvalue");
        assert!(s.contains("600519.CN-A\t1.5"));
    }

    #[test]
    fn table_aligns_and_no_trailing_space() {
        let s = to_string(Format::Table);
        for line in s.lines() {
            assert!(!line.ends_with(' '), "trailing space: {line:?}");
        }
    }

    #[test]
    fn json_is_ndjson_of_objects_with_empty_string_for_null() {
        let s = to_string(Format::Json);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        let v: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v["symbol"], "000001.CN-A");
        assert_eq!(v["value"], "");
    }
}
