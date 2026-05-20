//! `sift announce {types | list | show | download}`.
//!
//! Story 01 wires the subcommand surface and ships `types`. The other
//! three return `SiftError::Internal("...: not yet implemented")` so
//! `--help` lists them as documented commands but invocation makes the
//! "still pending" status obvious. Stories 02 / 03 / 04 fill them in.

#![allow(dead_code)]

use std::io::Write;

use clap::{Args, Subcommand};
use tabled::settings::{Padding, Style};
use tabled::{Table, Tabled};

use crate::domain::announcement::{categories, Category};
use crate::error::SiftError;
use crate::output::{self, Format, RenderRow};

/// Sentinel used in user-facing output for "no value here". Single
/// dash matches the convention `pandas` / `awk` users expect; sift
/// commands should never emit a bare empty string for a column that
/// is semantically nullable.
const MISSING: &str = "-";

#[derive(Subcommand, Debug)]
pub enum AnnounceCmd {
    /// Print the 27 中文 `--type` values understood by `announce list`
    Types,
    /// List announcements for one or more symbols (Story 03)
    List(ListArgs),
    /// Show metadata for a single announcement (Story 03)
    Show(ShowArgs),
    /// Download PDFs to a local directory (Story 04)
    Download(DownloadArgs),
}

/// `sift announce list <symbol>...` — wired in Story 03.
#[derive(Args, Debug)]
pub struct ListArgs {
    /// One or more symbols (e.g. `600519`, `00700`).
    #[arg(required = true)]
    pub symbols: Vec<String>,
}

/// `sift announce show <announcement_id>` — wired in Story 03.
#[derive(Args, Debug)]
pub struct ShowArgs {
    /// `announcementId`, e.g. `1219506510`.
    pub announcement_id: String,
}

/// `sift announce download <announcement_id>... -o <dir>` — wired in Story 04.
#[derive(Args, Debug)]
pub struct DownloadArgs {
    /// One or more `announcementId`s.
    #[arg(required = true)]
    pub announcement_ids: Vec<String>,
}

pub fn run(cmd: AnnounceCmd, fmt: Format) -> Result<(), SiftError> {
    match cmd {
        AnnounceCmd::Types => run_types(fmt),
        AnnounceCmd::List(_) => Err(SiftError::Internal(
            "announce list: not yet implemented (Story 03)".into(),
        )),
        AnnounceCmd::Show(_) => Err(SiftError::Internal(
            "announce show: not yet implemented (Story 03)".into(),
        )),
        AnnounceCmd::Download(_) => Err(SiftError::Internal(
            "announce download: not yet implemented (Story 04)".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// `types` subcommand
// ---------------------------------------------------------------------------

/// One row of `sift announce types` output. Owned strings — they come
/// from the runtime-deserialized dictionary, so we cannot promise
/// `&'static str`. Empty slots are filled with [`MISSING`] (`-`)
/// instead of an empty string so all three output formats render
/// consistently and downstream tools (pandas, awk) get an explicit
/// "no value" marker.
///
/// Two derives coexist on this struct:
/// - `Tabled` powers the human-facing borderless table (the
///   `--format table` path, which is also the bare default).
/// - `Serialize` + `RenderRow` keep the existing TSV / NDJSON paths
///   going through `output::render`.
#[derive(Debug, Clone, serde::Serialize, Tabled)]
struct TypeRow {
    zh: String,
    category: String,
    note: String,
}

impl RenderRow for TypeRow {
    fn headers() -> &'static [&'static str] {
        &["zh", "category", "note"]
    }

    fn cells(&self) -> Vec<String> {
        vec![self.zh.clone(), self.category.clone(), self.note.clone()]
    }
}

fn run_types(fmt: Format) -> Result<(), SiftError> {
    let rows: Vec<TypeRow> = categories().iter().map(to_type_row).collect();
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    match fmt {
        // Default human-facing path: borderless `tabled` table. Two
        // characters of right padding give the same visual rhythm as
        // sift's other commands, which still hand-roll alignment.
        Format::Table => render_tabled(&mut handle, &rows),
        // TSV / NDJSON stay on the generic RenderRow pipeline; their
        // semantics (tab-separated body, one JSON object per line)
        // are unaffected by the table-only restyling.
        Format::Tsv | Format::Ndjson => output::render(&mut handle, fmt, &rows),
    }
}

/// Render rows through the `tabled` crate with `Style::empty()` and
/// `Padding(0, 2, 0, 0)` — borderless, two-space inter-column gap,
/// no top/bottom padding. Mirrors the F2 tech_design tabled
/// convention and keeps the visual identity uniform across sift
/// commands.
fn render_tabled<W: Write>(out: &mut W, rows: &[TypeRow]) -> Result<(), SiftError> {
    let mut t = Table::new(rows);
    t.with(Style::empty()).with(Padding::new(0, 2, 0, 0));
    writeln!(out, "{t}").map_err(|e| SiftError::Internal(format!("io: {e}")))?;
    Ok(())
}

/// Map a `Category` to one row of the `types` output. Both "missing"
/// slots — `category` for an aggregate, `note` for an official entry
/// — get [`MISSING`] (`-`) so the table reads symmetrically and
/// machine consumers see an explicit non-empty token.
fn to_type_row(c: &Category) -> TypeRow {
    match (&c.category, &c.aggregates) {
        (Some(cat), None) => TypeRow {
            zh: c.zh.clone(),
            category: cat.clone(),
            note: MISSING.into(),
        },
        (None, Some(aggs)) => TypeRow {
            zh: c.zh.clone(),
            category: MISSING.into(),
            note: format!("aggregates {}", aggs.join(" + ")),
        },
        _ => {
            // `load_categories` rejects this shape during validation;
            // a panic here means the dict drifted out of sync with
            // the parser — surface it loudly rather than emit a
            // half-rendered row.
            panic!("category {:?} has invalid shape", c.zh);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_returns_internal_error_for_unimplemented_subcommands() {
        let err = run(AnnounceCmd::List(ListArgs { symbols: vec!["x".into()] }), Format::Table)
            .unwrap_err();
        assert!(matches!(err, SiftError::Internal(_)));
        assert!(err.to_string().contains("not yet implemented"));
        assert_eq!(err.exit_code(), 1);

        let err = run(
            AnnounceCmd::Show(ShowArgs {
                announcement_id: "1".into(),
            }),
            Format::Table,
        )
        .unwrap_err();
        assert!(matches!(err, SiftError::Internal(_)));

        let err = run(
            AnnounceCmd::Download(DownloadArgs {
                announcement_ids: vec!["1".into()],
            }),
            Format::Table,
        )
        .unwrap_err();
        assert!(matches!(err, SiftError::Internal(_)));
    }

    #[test]
    fn to_type_row_uses_dash_placeholder_for_empty_note() {
        let cat = Category {
            zh: "年报".into(),
            category: Some("category_ndbg_szsh".into()),
            aggregates: None,
        };
        let r = to_type_row(&cat);
        assert_eq!(r.zh, "年报");
        assert_eq!(r.category, "category_ndbg_szsh");
        assert_eq!(r.note, MISSING);
    }

    #[test]
    fn to_type_row_uses_dash_placeholder_for_aggregate_category() {
        let cat = Category {
            zh: "定期报告".into(),
            category: None,
            aggregates: Some(vec![
                "年报".into(),
                "半年报".into(),
                "一季报".into(),
                "三季报".into(),
            ]),
        };
        let r = to_type_row(&cat);
        assert_eq!(r.zh, "定期报告");
        assert_eq!(r.category, MISSING);
        assert!(r.note.starts_with("aggregates "));
        assert!(r.note.contains("年报"));
        assert!(r.note.contains("三季报"));
    }

    #[test]
    fn run_types_rows_match_dictionary_shape() {
        let rows: Vec<TypeRow> = categories().iter().map(to_type_row).collect();
        assert_eq!(rows.len(), 27);
        // First entry: official `年报` with `-` in note.
        assert_eq!(rows[0].zh, "年报");
        assert_eq!(rows[0].category, "category_ndbg_szsh");
        assert_eq!(rows[0].note, MISSING);
        // Last entry: aggregate `定期报告` with `-` in category.
        assert_eq!(rows[26].zh, "定期报告");
        assert_eq!(rows[26].category, MISSING);
        assert!(rows[26].note.contains("年报"));
    }

    #[test]
    fn table_format_uses_tabled_borderless_style() {
        let rows: Vec<TypeRow> = categories().iter().map(to_type_row).collect();
        let mut buf = Vec::<u8>::new();
        render_tabled(&mut buf, &rows).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Borderless: no box-drawing characters anywhere. (`+` is
        // excluded because the aggregate-row note legitimately
        // contains it, e.g. `aggregates 年报 + 半年报 + ...`.)
        for ch in ['|', '─', '│', '┌', '┐', '└', '┘'] {
            assert!(
                !s.contains(ch),
                "borderless table should not contain {ch:?}: {s}"
            );
        }
        // Header from `Tabled` derive uses field names verbatim.
        let first_line = s.lines().next().unwrap();
        assert!(first_line.contains("zh"));
        assert!(first_line.contains("category"));
        assert!(first_line.contains("note"));
        // Body contains expected dictionary entries + dash placeholder.
        assert!(s.contains("年报"));
        assert!(s.contains("定期报告"));
        assert!(s.contains(MISSING));
        assert!(s.contains("category_ndbg_szsh"));
    }

    #[test]
    fn tsv_and_ndjson_paths_still_use_render_pipeline() {
        // RenderRow contract: TSV / NDJSON go through `output::render`.
        let rows: Vec<TypeRow> = categories().iter().map(to_type_row).collect();
        for fmt in [Format::Tsv, Format::Ndjson] {
            let mut buf = Vec::<u8>::new();
            output::render(&mut buf, fmt, &rows).unwrap();
            let s = String::from_utf8(buf).unwrap();
            assert!(!s.is_empty(), "{fmt:?} produced no output");
            assert!(s.contains("年报"));
            assert!(s.contains("定期报告"));
            // Dash placeholder reaches all formats.
            assert!(s.contains(MISSING));
        }
    }
}
