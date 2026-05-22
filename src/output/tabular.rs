//! Cross-command flat-TSV output convention.
//!
//! A view declares its schema via [`TabularView`]; [`render_tabular`] is
//! the lone byte-level emitter that writes a `#col1\tcol2\t…\n` header
//! followed by tab-separated rows. Any command that emits
//! `--format tsv` should delegate to this helper so the project has one
//! place that owns separator / header-prefix / control-char policy.
//!
//! Pattern adopted from `bio-ncbi`'s `render::document::tabular` trait
//! ([crates/bio-ncbi-core/src/render/document/tabular.rs] in that
//! repo) — same trait shape so a future cross-tool migration is a
//! `use` swap.
//!
//! ## Convention
//!
//! - Header line: `#col1\tcol2\t…\n` with **no space** after `#`.
//!   `pandas.read_csv(sep='\t', comment='#')` and `awk` both treat the
//!   line as a comment regardless, and the no-space form keeps a single
//!   shape across tools.
//! - Data rows: tab-separated, one row per line.
//! - Cell content with `\t` / `\n` / `\r` returns
//!   `Err(SiftError::Internal)`. sift's TSV inputs come from controlled
//!   vocabularies (item dictionary + secucodes); a control char would
//!   be a bug, so we surface it rather than silently sanitize.
//! - `include_header() == false` suppresses the entire header line
//!   (reserved for a future `--no-header` flag).
//! - Empty `columns()` writes nothing — a bare `#\n` line would be
//!   misleading.

use std::io::Write;

use crate::error::SiftError;
use crate::output::io_err;

/// Views that emit a flat TSV body declare their schema here. The only
/// approved consumer is [`render_tabular`]; never hand-roll a TSV header
/// in a command.
pub trait TabularView {
    /// Column names in row order. Length must match every row produced
    /// by [`rows`](TabularView::rows). Returns `Vec<&str>` so views
    /// with runtime-resolved schemas (e.g. financials' item set across
    /// multiple symbols) can splice from owned fields each call.
    fn columns(&self) -> Vec<&str>;

    /// One `Vec<String>` per data row. Each row's length must equal
    /// `columns().len()` — `render_tabular` `debug_assert_eq!`s this
    /// to catch ragged-row bugs in test builds.
    fn rows(&self) -> Vec<Vec<String>>;

    /// Whether the leading `#col1\tcol2\t…` header line is emitted.
    fn include_header(&self) -> bool {
        true
    }
}

/// Emit a [`TabularView`] in canonical form. The single approved writer
/// of the project-wide `#col1\tcol2\t…\n` header.
pub fn render_tabular<W: Write>(out: &mut W, view: &dyn TabularView) -> Result<(), SiftError> {
    let cols = view.columns();
    if cols.is_empty() {
        return Ok(());
    }
    if view.include_header() {
        out.write_all(b"#").map_err(io_err)?;
        for (i, c) in cols.iter().enumerate() {
            if i > 0 {
                out.write_all(b"\t").map_err(io_err)?;
            }
            check_cell(c)?;
            out.write_all(c.as_bytes()).map_err(io_err)?;
        }
        out.write_all(b"\n").map_err(io_err)?;
    }
    let col_count = cols.len();
    for row in view.rows() {
        debug_assert_eq!(
            row.len(),
            col_count,
            "TabularView::rows produced a ragged row",
        );
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.write_all(b"\t").map_err(io_err)?;
            }
            check_cell(cell)?;
            out.write_all(cell.as_bytes()).map_err(io_err)?;
        }
        out.write_all(b"\n").map_err(io_err)?;
    }
    Ok(())
}

fn check_cell(s: &str) -> Result<(), SiftError> {
    if s.contains(['\t', '\n', '\r']) {
        return Err(SiftError::Internal(
            "tabular: control char in cell".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        cols: Vec<&'static str>,
        rows: Vec<Vec<String>>,
        header: bool,
    }
    impl TabularView for Fixture {
        fn columns(&self) -> Vec<&str> {
            self.cols.clone()
        }
        fn rows(&self) -> Vec<Vec<String>> {
            self.rows.clone()
        }
        fn include_header(&self) -> bool {
            self.header
        }
    }

    fn render_to_string(v: &dyn TabularView) -> String {
        let mut buf = Vec::<u8>::new();
        render_tabular(&mut buf, v).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn header_has_no_space_after_hash() {
        let v = Fixture {
            cols: vec!["cid", "iupac-name", "mw"],
            rows: vec![vec!["2244".into(), "aspirin".into(), "180.16".into()]],
            header: true,
        };
        let out = render_to_string(&v);
        let first = out.lines().next().unwrap();
        assert_eq!(first, "#cid\tiupac-name\tmw");
        assert!(!first.starts_with("# "));
    }

    #[test]
    fn omits_header_when_include_header_false() {
        let v = Fixture {
            cols: vec!["cid"],
            rows: vec![vec!["1".into()], vec!["2".into()]],
            header: false,
        };
        let out = render_to_string(&v);
        assert!(!out.starts_with('#'), "got {out:?}");
        assert_eq!(out, "1\n2\n");
    }

    #[test]
    fn emits_one_row_per_line_tab_separated() {
        let v = Fixture {
            cols: vec!["a", "b"],
            rows: vec![
                vec!["1".into(), "2".into()],
                vec!["3".into(), "4".into()],
            ],
            header: true,
        };
        assert_eq!(render_to_string(&v), "#a\tb\n1\t2\n3\t4\n");
    }

    #[test]
    fn errors_on_tab_in_cell() {
        let v = Fixture {
            cols: vec!["a"],
            rows: vec![vec!["x\ty".into()]],
            header: false,
        };
        let mut buf = Vec::<u8>::new();
        let err = render_tabular(&mut buf, &v).unwrap_err();
        assert!(format!("{err}").contains("control char"));
    }

    #[test]
    fn errors_on_newline_in_cell() {
        let v = Fixture {
            cols: vec!["a"],
            rows: vec![vec!["x\ny".into()]],
            header: false,
        };
        let mut buf = Vec::<u8>::new();
        assert!(render_tabular(&mut buf, &v).is_err());
    }

    #[test]
    fn errors_on_tab_in_column_name() {
        let v = Fixture {
            cols: vec!["bad\tname"],
            rows: vec![],
            header: true,
        };
        let mut buf = Vec::<u8>::new();
        assert!(render_tabular(&mut buf, &v).is_err());
    }

    #[test]
    fn empty_columns_writes_nothing() {
        let v = Fixture {
            cols: vec![],
            rows: vec![],
            header: true,
        };
        assert_eq!(render_to_string(&v), "");
    }
}
