//! `sift announce {types | list | show | download}`.
//!
//! Story 01 wires the subcommand surface and ships `types`. The other
//! three return `SiftError::Internal("...: not yet implemented")` so
//! `--help` lists them as documented commands but invocation makes the
//! "still pending" status obvious. Stories 02 / 03 / 04 fill them in.

#![allow(dead_code)]

use clap::{Args, Subcommand};

use crate::domain::announcement::{categories, Category};
use crate::error::SiftError;
use crate::output::{self, Format, RenderRow};

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
/// `&'static str`.
#[derive(Debug, Clone, serde::Serialize)]
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
    output::render(&mut handle, fmt, &rows)
}

/// Map a `Category` to one row of the `types` output. For aggregates,
/// `category` is the sentinel `"(本地聚合)"` and `note` lists the
/// rolled-up `zh` values so the user can copy-paste any of them.
fn to_type_row(c: &Category) -> TypeRow {
    match (&c.category, &c.aggregates) {
        (Some(cat), None) => TypeRow {
            zh: c.zh.clone(),
            category: cat.clone(),
            note: String::new(),
        },
        (None, Some(aggs)) => TypeRow {
            zh: c.zh.clone(),
            category: "(本地聚合)".into(),
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
    fn to_type_row_preserves_official_category_with_empty_note() {
        let cat = Category {
            zh: "年报".into(),
            category: Some("category_ndbg_szsh".into()),
            aggregates: None,
        };
        let r = to_type_row(&cat);
        assert_eq!(r.zh, "年报");
        assert_eq!(r.category, "category_ndbg_szsh");
        assert!(r.note.is_empty());
    }

    #[test]
    fn to_type_row_flags_aggregate_with_sentinel_category() {
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
        assert_eq!(r.category, "(本地聚合)");
        assert!(r.note.starts_with("aggregates "));
        assert!(r.note.contains("年报"));
        assert!(r.note.contains("三季报"));
    }

    #[test]
    fn run_types_renders_27_rows_to_table() {
        // Capture stdout would require redirecting; instead, exercise
        // the row construction + RenderRow contract directly.
        let rows: Vec<TypeRow> = categories().iter().map(to_type_row).collect();
        assert_eq!(rows.len(), 27);
        // First and last match the dictionary shape.
        assert_eq!(rows[0].zh, "年报");
        assert_eq!(rows[0].category, "category_ndbg_szsh");
        assert_eq!(rows[26].zh, "定期报告");
        assert_eq!(rows[26].category, "(本地聚合)");

        // Render through every format to confirm RenderRow wiring.
        for fmt in [Format::Table, Format::Tsv, Format::Ndjson] {
            let mut buf = Vec::<u8>::new();
            output::render(&mut buf, fmt, &rows).unwrap();
            let s = String::from_utf8(buf).unwrap();
            assert!(!s.is_empty(), "{fmt:?} produced no output");
            // Every output mentions both `年报` and the aggregate sentinel.
            assert!(s.contains("年报"));
            assert!(s.contains("定期报告"));
        }
    }
}
