//! Render layer for `sift announce`: projection structs + the three
//! renderers (`types`, `list`, `show`). Sibling of
//! [`crate::output::financial_render`] for F2; consumed by
//! [`crate::commands::announce`]. No cache, no network — every
//! function here takes already-resolved domain values and writes
//! formatted bytes to the provided `W`.

use std::io::Write;

use serde::Serialize;
use tabled::settings::{Padding, Style};
use tabled::{Table, Tabled};
use time::format_description::well_known::Iso8601;

use crate::domain::announcement::{AnnouncementRow, Category};
use crate::error::SiftError;
use crate::output::tabular::TabularView;
use crate::output::RenderRow;

/// Sentinel used in user-facing output for "no value here". Single
/// dash matches the convention `pandas` / `awk` users expect; sift
/// commands should never emit a bare empty string for a column that
/// is semantically nullable.
const MISSING: &str = "-";

// ---------------------------------------------------------------------------
// `types` view
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
#[derive(Debug, Clone, Serialize, Tabled)]
pub struct TypeRow {
    pub zh: String,
    pub category: String,
    pub note: String,
}

impl RenderRow for TypeRow {
    fn headers() -> &'static [&'static str] {
        &["zh", "category", "note"]
    }

    fn cells(&self) -> Vec<String> {
        vec![self.zh.clone(), self.category.clone(), self.note.clone()]
    }
}

/// Map a `Category` to one row of the `types` output. Both "missing"
/// slots — `category` for an aggregate, `note` for an official entry
/// — get [`MISSING`] (`-`) so the table reads symmetrically and
/// machine consumers see an explicit non-empty token.
pub fn to_type_row(c: &Category) -> TypeRow {
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

/// Render rows through the `tabled` crate with `Style::empty()` and
/// `Padding(0, 2, 0, 0)` — borderless, two-space inter-column gap,
/// no top/bottom padding. Mirrors the F2 tech_design tabled
/// convention and keeps the visual identity uniform across sift
/// commands.
pub fn render_tabled<W: Write>(out: &mut W, rows: &[TypeRow]) -> Result<(), SiftError> {
    let mut t = Table::new(rows);
    t.with(Style::empty()).with(Padding::new(0, 2, 0, 0));
    writeln!(out, "{t}").map_err(|e| SiftError::Internal(format!("io: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// `list` view
// ---------------------------------------------------------------------------

/// TSV view of an `announce list` result. Columns mirror
/// `AnnouncementRow::headers()` so `--format tsv` shape is byte-for-
/// byte stable across the cninfo schema; only the header prefix
/// (`#`) changes when migrating from `output::render`.
pub struct AnnouncementListTsvView<'a>(pub &'a [AnnouncementRow]);

impl TabularView for AnnouncementListTsvView<'_> {
    fn columns(&self) -> Vec<&str> {
        AnnouncementRow::headers().to_vec()
    }
    fn rows(&self) -> Vec<Vec<String>> {
        self.0.iter().map(|r| r.cells()).collect()
    }
}

/// Borderless `tabled` table for `announce list`. We project
/// `AnnouncementRow` down to seven visible columns (the README order:
/// `id / symbol / name / date / type / title / size_kb`) to keep the
/// TTY width manageable; TSV / NDJSON still emit all ten via the
/// generic `RenderRow` path.
pub fn render_list_table<W: Write>(
    out: &mut W,
    rows: &[AnnouncementRow],
) -> Result<(), SiftError> {
    let projected: Vec<ListTableRow> = rows.iter().map(ListTableRow::from_row).collect();
    let mut t = Table::new(&projected);
    t.with(Style::empty()).with(Padding::new(0, 2, 0, 0));
    writeln!(out, "{t}").map_err(|e| SiftError::Internal(format!("io: {e}")))?;
    Ok(())
}

/// Seven-column projection of [`AnnouncementRow`] for the `tabled`
/// TTY renderer. `type` is a Rust keyword so the field is named
/// `type_zh` and re-labelled via `#[tabled(rename = ...)]`.
#[derive(Tabled)]
struct ListTableRow {
    id: String,
    symbol: String,
    name: String,
    date: String,
    #[tabled(rename = "type")]
    type_zh: String,
    title: String,
    size_kb: u32,
}

impl ListTableRow {
    fn from_row(r: &AnnouncementRow) -> Self {
        Self {
            id: r.id.clone(),
            symbol: r.symbol.clone(),
            name: r.name.clone(),
            date: r.date.format(&Iso8601::DATE).unwrap_or_default(),
            type_zh: r.type_zh.clone(),
            title: r.title.clone(),
            size_kb: r.size_kb,
        }
    }
}

// ---------------------------------------------------------------------------
// `show` view
// ---------------------------------------------------------------------------

/// One-row payload for `show` in TSV / NDJSON. Owns its strings — the
/// row that fed in came from stdin and is cheap to clone once.
#[derive(Serialize)]
pub struct ShowOutRow {
    pub id: String,
    pub symbol: String,
    pub name: String,
    pub date: String,
    #[serde(rename = "type")]
    pub type_zh: String,
    pub title: String,
    pub format: String,
    pub size_kb: u32,
    pub url: String,
    pub source: String,
    pub cached: String,
}

impl ShowOutRow {
    pub fn from_parts(row: &AnnouncementRow, cached: &str) -> Self {
        Self {
            id: row.id.clone(),
            symbol: row.symbol.clone(),
            name: row.name.clone(),
            date: row.date.format(&Iso8601::DATE).unwrap_or_default(),
            type_zh: row.type_zh.clone(),
            title: row.title.clone(),
            format: row.format.into(),
            size_kb: row.size_kb,
            url: row.url.clone(),
            source: row.source.into(),
            cached: cached.into(),
        }
    }
}

impl RenderRow for ShowOutRow {
    fn headers() -> &'static [&'static str] {
        &[
            "id", "symbol", "name", "date", "type", "title", "format", "size_kb", "url", "source",
            "cached",
        ]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.id.clone(),
            self.symbol.clone(),
            self.name.clone(),
            self.date.clone(),
            self.type_zh.clone(),
            self.title.clone(),
            self.format.clone(),
            self.size_kb.to_string(),
            self.url.clone(),
            self.source.clone(),
            self.cached.clone(),
        ]
    }
}

/// TSV view of the single-row `show` payload. The 11-column schema
/// matches [`ShowOutRow::headers`]; only the header prefix differs
/// from the NDJSON-companion `RenderRow` path.
pub struct ShowTsvView<'a>(pub &'a ShowOutRow);

impl TabularView for ShowTsvView<'_> {
    fn columns(&self) -> Vec<&str> {
        ShowOutRow::headers().to_vec()
    }
    fn rows(&self) -> Vec<Vec<String>> {
        vec![self.0.cells()]
    }
}

/// Two-column `key  value` view for `show`. Keys are left-justified
/// in unicode width; values follow after a two-space gutter — the
/// same rhythm as `tabled`'s `Padding::new(0, 2, 0, 0)`.
pub fn render_show_kv<W: Write>(
    out: &mut W,
    row: &AnnouncementRow,
    cached: &str,
) -> Result<(), SiftError> {
    let pairs: [(&str, String); 11] = [
        ("id", row.id.clone()),
        ("symbol", row.symbol.clone()),
        ("name", row.name.clone()),
        ("date", row.date.format(&Iso8601::DATE).unwrap_or_default()),
        ("type", row.type_zh.clone()),
        ("title", row.title.clone()),
        ("format", row.format.into()),
        ("size_kb", row.size_kb.to_string()),
        ("url", row.url.clone()),
        ("source", row.source.into()),
        ("cached", cached.into()),
    ];
    let key_w = pairs.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in &pairs {
        writeln!(out, "{k:<key_w$}  {v}")
            .map_err(|e| SiftError::Internal(format!("io: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::announcement::categories;
    use crate::output::{self, Format};
    use time::{Date, Month};

    fn d(y: i32, m: u8, day: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap()
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
        assert_eq!(rows[0].zh, "年报");
        assert_eq!(rows[0].category, "category_ndbg_szsh");
        assert_eq!(rows[0].note, MISSING);
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
        for ch in ['|', '─', '│', '┌', '┐', '└', '┘'] {
            assert!(
                !s.contains(ch),
                "borderless table should not contain {ch:?}: {s}"
            );
        }
        let first_line = s.lines().next().unwrap();
        assert!(first_line.contains("zh"));
        assert!(first_line.contains("category"));
        assert!(first_line.contains("note"));
        assert!(s.contains("年报"));
        assert!(s.contains("定期报告"));
        assert!(s.contains(MISSING));
        assert!(s.contains("category_ndbg_szsh"));
    }

    #[test]
    fn render_show_kv_emits_eleven_aligned_lines() {
        let row = AnnouncementRow {
            id: "1".into(),
            symbol: "600519.SH".into(),
            name: "贵州茅台".into(),
            date: d(2024, 4, 3),
            type_zh: "年报".into(),
            title: "T".into(),
            format: "pdf",
            size_kb: 9,
            url: "http://u".into(),
            source: "cninfo",
        };
        let mut buf = Vec::<u8>::new();
        render_show_kv(&mut buf, &row, "/p  (no)").unwrap();
        let s = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 11);
        assert!(lines[0].starts_with("id"));
        assert!(lines[10].starts_with("cached"));
        assert!(lines[10].contains("(no)"));
    }

    #[test]
    fn show_out_row_render_row_has_eleven_columns() {
        let row = AnnouncementRow {
            id: "1".into(),
            symbol: "600519.SH".into(),
            name: "x".into(),
            date: d(2024, 4, 3),
            type_zh: "年报".into(),
            title: "t".into(),
            format: "pdf",
            size_kb: 0,
            url: "".into(),
            source: "cninfo",
        };
        let out = ShowOutRow::from_parts(&row, "/p  (no)");
        assert_eq!(ShowOutRow::headers().len(), 11);
        assert_eq!(out.cells().len(), 11);
        assert_eq!(out.cells()[10], "/p  (no)");
    }

    #[test]
    fn tsv_and_ndjson_paths_still_use_render_pipeline() {
        let rows: Vec<TypeRow> = categories().iter().map(to_type_row).collect();
        for fmt in [Format::Tsv, Format::Json] {
            let mut buf = Vec::<u8>::new();
            output::render(&mut buf, fmt, &rows).unwrap();
            let s = String::from_utf8(buf).unwrap();
            assert!(!s.is_empty(), "{fmt:?} produced no output");
            assert!(s.contains("年报"));
            assert!(s.contains("定期报告"));
            assert!(s.contains(MISSING));
        }
    }
}
