use crate::error::SiftError;
use serde::Serialize;
use std::io::Write;
use unicode_width::UnicodeWidthStr;

/// A single renderable row.
///
/// - `headers()` provides the header row for table / tsv rendering;
/// - `cells()` provides the body cells for table / tsv (order must match headers);
/// - `Serialize` drives `serde_json::to_writer` for the ndjson renderer.
pub trait RenderRow: Serialize {
    fn headers() -> &'static [&'static str];
    fn cells(&self) -> Vec<String>;
}

/// Top-level dispatch: pick the concrete renderer for the requested
/// [`Format`](super::Format).
pub fn render<W: Write, R: RenderRow>(
    out: &mut W,
    fmt: super::Format,
    rows: &[R],
) -> Result<(), SiftError> {
    match fmt {
        super::Format::Table => render_table(out, rows),
        super::Format::Tsv => render_tsv(out, rows),
        super::Format::Ndjson => render_ndjson(out, rows),
    }
}

fn render_table<W: Write, R: RenderRow>(out: &mut W, rows: &[R]) -> Result<(), SiftError> {
    let headers = R::headers();
    let ncols = headers.len();
    let body: Vec<Vec<String>> = rows.iter().map(|r| r.cells()).collect();

    let mut widths = vec![0usize; ncols];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = UnicodeWidthStr::width(*h);
    }
    for row in &body {
        for (i, c) in row.iter().enumerate().take(ncols) {
            let w = UnicodeWidthStr::width(c.as_str());
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }

    write_table_row(out, &widths, headers.iter().copied())?;
    for row in &body {
        write_table_row(out, &widths, row.iter().map(String::as_str))?;
    }
    Ok(())
}

/// Write a single table row. The last column is **not** padded with
/// trailing spaces — keeps the output clean for diff review.
fn write_table_row<'a, W, I>(out: &mut W, widths: &[usize], cells: I) -> Result<(), SiftError>
where
    W: Write,
    I: Iterator<Item = &'a str>,
{
    let cells: Vec<&str> = cells.collect();
    let last = cells.len().saturating_sub(1);
    for (i, cell) in cells.iter().enumerate() {
        if i == last {
            out.write_all(cell.as_bytes()).map_err(io_err)?;
        } else {
            out.write_all(cell.as_bytes()).map_err(io_err)?;
            let pad = widths
                .get(i)
                .copied()
                .unwrap_or(0)
                .saturating_sub(UnicodeWidthStr::width(*cell));
            for _ in 0..pad {
                out.write_all(b" ").map_err(io_err)?;
            }
            out.write_all(b"  ").map_err(io_err)?;
        }
    }
    out.write_all(b"\n").map_err(io_err)?;
    Ok(())
}

fn render_tsv<W: Write, R: RenderRow>(out: &mut W, rows: &[R]) -> Result<(), SiftError> {
    let headers = R::headers();
    writeln!(out, "{}", headers.join("\t")).map_err(io_err)?;
    for row in rows {
        let cells = row.cells();
        for c in &cells {
            if c.contains('\t') || c.contains('\n') {
                return Err(SiftError::Internal(
                    "tsv: control char in cell".into(),
                ));
            }
        }
        writeln!(out, "{}", cells.join("\t")).map_err(io_err)?;
    }
    Ok(())
}

fn render_ndjson<W: Write, R: RenderRow>(out: &mut W, rows: &[R]) -> Result<(), SiftError> {
    for row in rows {
        serde_json::to_writer(&mut *out, row)
            .map_err(|e| SiftError::Internal(format!("ndjson serialize: {e}")))?;
        out.write_all(b"\n").map_err(io_err)?;
    }
    Ok(())
}

fn io_err(e: std::io::Error) -> SiftError {
    SiftError::Internal(format!("io: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Row {
        a: String,
        b: String,
        c: String,
    }

    impl RenderRow for Row {
        fn headers() -> &'static [&'static str] {
            &["a", "b", "c"]
        }
        fn cells(&self) -> Vec<String> {
            vec![self.a.clone(), self.b.clone(), self.c.clone()]
        }
    }

    fn sample() -> Vec<Row> {
        vec![
            Row {
                a: "1".into(),
                b: "long".into(),
                c: "x".into(),
            },
            Row {
                a: "22".into(),
                b: "a".into(),
                c: "yy".into(),
            },
        ]
    }

    #[test]
    fn table_has_header_and_aligned_columns() {
        let mut buf = Vec::<u8>::new();
        render_table(&mut buf, &sample()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 3, "header + 2 rows; got: {s:?}");
        assert!(lines[0].starts_with("a"), "header row missing 'a': {:?}", lines[0]);
        // Column "a" width 2 ('22'), column "b" width 4 ('long');
        // the third column has no trailing padding.
        assert_eq!(lines[1], "1   long  x");
        assert_eq!(lines[2], "22  a     yy");
    }

    #[test]
    fn table_rows_never_end_with_space() {
        let mut buf = Vec::<u8>::new();
        render_table(&mut buf, &sample()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        for line in s.lines() {
            assert!(!line.ends_with(' '), "trailing space in row: {line:?}");
        }
    }

    #[test]
    fn tsv_uses_tab_separator_with_header() {
        let mut buf = Vec::<u8>::new();
        render_tsv(&mut buf, &sample()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "a\tb\tc");
        assert_eq!(lines[1], "1\tlong\tx");
        assert_eq!(lines[2], "22\ta\tyy");
    }

    #[test]
    fn tsv_rejects_tab_inside_cell() {
        let bad = vec![Row {
            a: "ok".into(),
            b: "has\ttab".into(),
            c: "x".into(),
        }];
        let mut buf = Vec::<u8>::new();
        let err = render_tsv(&mut buf, &bad).unwrap_err();
        match err {
            SiftError::Internal(m) => assert!(m.contains("tsv"), "msg: {m}"),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn tsv_rejects_newline_inside_cell() {
        let bad = vec![Row {
            a: "ok".into(),
            b: "has\nnl".into(),
            c: "x".into(),
        }];
        let mut buf = Vec::<u8>::new();
        assert!(matches!(render_tsv(&mut buf, &bad), Err(SiftError::Internal(_))));
    }

    #[test]
    fn ndjson_one_object_per_line_no_array_wrapper() {
        let mut buf = Vec::<u8>::new();
        render_ndjson(&mut buf, &sample()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.starts_with('['), "ndjson must not be wrapped in array: {s:?}");
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
            assert!(v.is_object(), "each ndjson line is an object: {line}");
        }
    }

    #[test]
    fn ndjson_streamable_via_deserializer() {
        let mut buf = Vec::<u8>::new();
        render_ndjson(&mut buf, &sample()).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::Deserializer::from_slice(&buf)
            .into_iter::<serde_json::Value>()
            .collect::<Result<_, _>>()
            .expect("streaming deserializer parses ndjson");
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn render_dispatches_by_format() {
        // render() is pure dispatch; this is a smoke test that all three
        // formats produce non-empty output for the same input.
        for fmt in [
            super::super::Format::Table,
            super::super::Format::Tsv,
            super::super::Format::Ndjson,
        ] {
            let mut buf = Vec::<u8>::new();
            render(&mut buf, fmt, &sample()).unwrap();
            assert!(!buf.is_empty(), "format {fmt:?} produced no output");
        }
    }
}
