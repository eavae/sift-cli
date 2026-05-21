//! Announcement domain types + cninfo `--type` dictionary.
//!
//! `AnnouncementRow` is the long-format row Story 02 / 03 / 04 produce
//! and render. `Category` + [`categories`] + [`lookup`] expose the
//! 26 official cninfo `category_*` values plus the one local aggregate
//! `定期报告` used by `sift announce list --type 定期报告`.
//!
//! The dictionary ships as `data/announcement_categories.json` and is
//! embedded into the binary via `include_str!`; there is no runtime
//! file lookup. Validation (every `aggregates` member must point at an
//! existing `zh` row) runs on first access — a malformed shipped JSON
//! panics there.

use std::collections::HashSet;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

const CATEGORIES_JSON: &str = include_str!("../../data/announcement_categories.json");

/// One row from the cninfo announcements list, post-translation.
///
/// `format` and `source` are fixed strings today (`"pdf"` /
/// `"cninfo"`); kept as `&'static str` so the struct stays cheap to
/// clone and we don't pay per-row allocation for constants. `date`
/// serializes as `YYYY-MM-DD` via a custom `serialize_with` so we do
/// not require the time crate's `serde` feature. The `Deserialize`
/// path (used by the record cache to revive a stored row) is paired
/// with `deserialize_*` helpers that map any input to the canonical
/// `&'static str` literals — keeping the in-memory representation
/// allocation-free regardless of round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnouncementRow {
    pub id: String,
    pub symbol: String,
    pub name: String,
    #[serde(
        serialize_with = "serialize_iso_date",
        deserialize_with = "deserialize_iso_date"
    )]
    pub date: time::Date,
    #[serde(rename = "type")]
    pub type_zh: String,
    pub title: String,
    #[serde(skip_deserializing, default = "pdf_literal")]
    pub format: &'static str,
    pub size_kb: u32,
    pub url: String,
    #[serde(skip_deserializing, default = "cninfo_literal")]
    pub source: &'static str,
}

fn serialize_iso_date<S: serde::Serializer>(d: &time::Date, s: S) -> Result<S::Ok, S::Error> {
    let str_form = d
        .format(&time::format_description::well_known::Iso8601::DATE)
        .map_err(serde::ser::Error::custom)?;
    s.serialize_str(&str_form)
}

fn deserialize_iso_date<'de, D: serde::Deserializer<'de>>(d: D) -> Result<time::Date, D::Error> {
    let s = String::deserialize(d)?;
    time::Date::parse(&s, &time::format_description::well_known::Iso8601::DATE)
        .map_err(serde::de::Error::custom)
}

/// Default for `format` during deserialization. The field is
/// `skip_deserializing`'d so any drift in the stored bytes is ignored
/// and the canonical literal is restored; this also lets the field
/// stay `&'static str` (a derived `Deserialize` would propagate a
/// `'de: 'static` bound through the whole struct).
fn pdf_literal() -> &'static str {
    "pdf"
}

/// Mirror of [`pdf_literal`] for the `source` field.
fn cninfo_literal() -> &'static str {
    "cninfo"
}

impl crate::output::RenderRow for AnnouncementRow {
    fn headers() -> &'static [&'static str] {
        &[
            "id", "symbol", "name", "date", "type", "title", "format", "size_kb", "url",
            "source",
        ]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.id.clone(),
            self.symbol.clone(),
            self.name.clone(),
            self.date
                .format(&time::format_description::well_known::Iso8601::DATE)
                .unwrap_or_default(),
            self.type_zh.clone(),
            self.title.clone(),
            self.format.into(),
            self.size_kb.to_string(),
            self.url.clone(),
            self.source.into(),
        ]
    }
}

// ---------------------------------------------------------------------------
// Category dictionary
// ---------------------------------------------------------------------------

/// One row of the `--type` dictionary. Each entry has exactly one of
/// `category` (cninfo's `category_*` key) or `aggregates` (local
/// fan-out to multiple `zh` values, e.g. `定期报告 = 年报 + 半年报 + 一季报 + 三季报`).
#[derive(Debug, Clone, Deserialize)]
pub struct Category {
    pub zh: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub aggregates: Option<Vec<String>>,
}

/// The compiled-in dictionary. Lazily parsed + validated on first
/// access. Order matches the README table.
pub fn categories() -> &'static [Category] {
    static C: OnceLock<Vec<Category>> = OnceLock::new();
    C.get_or_init(|| {
        load_categories(CATEGORIES_JSON)
            .expect("data/announcement_categories.json failed to load")
    })
    .as_slice()
}

/// Look up by the user-facing Chinese name (`--type` value). Returns
/// `None` for any string not in the dictionary — callers (clap value
/// parser, F3 list / show) treat that as a usage error.
pub fn lookup(zh: &str) -> Option<&'static Category> {
    categories().iter().find(|c| c.zh == zh)
}

/// Reverse lookup: given a cninfo `columnId` (e.g.
/// `category_ndbg_szsh`), return the matching dictionary entry.
/// Used by Story 02 to translate `RawAnnouncement.columnId` into the
/// `AnnouncementRow.type_zh` field. Aggregate entries never match
/// (they have no `category` key) so they are naturally skipped.
pub fn lookup_by_key(cninfo_key: &str) -> Option<&'static Category> {
    categories()
        .iter()
        .find(|c| c.category.as_deref() == Some(cninfo_key))
}

/// Parse + validate the categories JSON. Each `aggregates` member
/// must point at an existing `zh` in the same file; orphaned
/// references would silently swallow `--type` queries at runtime.
fn load_categories(json: &str) -> Result<Vec<Category>, String> {
    let cats: Vec<Category> =
        serde_json::from_str(json).map_err(|e| format!("parse: {e}"))?;
    validate_categories(&cats)?;
    Ok(cats)
}

fn validate_categories(cats: &[Category]) -> Result<(), String> {
    if cats.is_empty() {
        return Err("empty categories list".into());
    }
    let zh_set: HashSet<&str> = cats.iter().map(|c| c.zh.as_str()).collect();
    if zh_set.len() != cats.len() {
        return Err("duplicate `zh` entry".into());
    }
    for cat in cats {
        match (cat.category.as_deref(), cat.aggregates.as_ref()) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "category {:?} has both `category` and `aggregates`; pick one",
                    cat.zh
                ));
            }
            (None, None) => {
                return Err(format!(
                    "category {:?} has neither `category` nor `aggregates`",
                    cat.zh
                ));
            }
            (None, Some(aggs)) => {
                if aggs.is_empty() {
                    return Err(format!("category {:?}: empty aggregates list", cat.zh));
                }
                for a in aggs {
                    if !zh_set.contains(a.as_str()) {
                        return Err(format!(
                            "category {:?}: aggregate {a:?} not found in dictionary",
                            cat.zh
                        ));
                    }
                    if a == &cat.zh {
                        return Err(format!(
                            "category {:?}: cannot aggregate itself",
                            cat.zh
                        ));
                    }
                }
            }
            (Some(_), None) => {} // ordinary entry
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::RenderRow;
    use time::{Date, Month};

    fn d(y: i32, m: u8, day: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap()
    }

    fn sample_row() -> AnnouncementRow {
        AnnouncementRow {
            id: "1219506510".into(),
            symbol: "600519.SH".into(),
            name: "贵州茅台".into(),
            date: d(2024, 4, 3),
            type_zh: "年报".into(),
            title: "贵州茅台2023年年度报告".into(),
            format: "pdf",
            size_kb: 3481,
            url: "http://static.cninfo.com.cn/finalpage/2024-04-03/1219506510.PDF".into(),
            source: "cninfo",
        }
    }

    #[test]
    fn announcement_row_serde_round_trip_preserves_fields() {
        // Round-trip via the record-cache path: serialize → bytes →
        // deserialize → struct. `format`/`source` must come back as
        // the canonical `&'static str` literals regardless of the
        // serialized text.
        let original = sample_row();
        let bytes = serde_json::to_vec(&original).unwrap();
        let restored: AnnouncementRow = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.symbol, original.symbol);
        assert_eq!(restored.name, original.name);
        assert_eq!(restored.date, original.date);
        assert_eq!(restored.type_zh, original.type_zh);
        assert_eq!(restored.title, original.title);
        assert_eq!(restored.size_kb, original.size_kb);
        assert_eq!(restored.url, original.url);
        // Literal-mapped fields: storage byte-equality is a happy
        // accident; the contract is the static-str identity.
        assert_eq!(restored.format, "pdf");
        assert_eq!(restored.source, "cninfo");
    }

    #[test]
    fn deserialize_format_and_source_force_canonical_literals() {
        // A drifted stored row (someone hand-edited the DB; a future
        // schema migration left a stale value) must still come back
        // as the canonical literals.
        let drifted = r#"{
            "id": "x", "symbol": "y", "name": "z", "date": "2024-01-01",
            "type": "t", "title": "T", "format": "html", "size_kb": 0,
            "url": "u", "source": "other-source"
        }"#;
        let row: AnnouncementRow = serde_json::from_str(drifted).unwrap();
        assert_eq!(row.format, "pdf");
        assert_eq!(row.source, "cninfo");
    }

    #[test]
    fn shipped_dict_has_27_entries_with_aggregate_last() {
        let cats = categories();
        assert_eq!(cats.len(), 27, "27 entries (26 official + 1 aggregate)");
        // First entry is `年报` per README order.
        assert_eq!(cats[0].zh, "年报");
        assert_eq!(cats[0].category.as_deref(), Some("category_ndbg_szsh"));
        // Last entry is the local aggregate.
        let last = &cats[26];
        assert_eq!(last.zh, "定期报告");
        assert!(last.aggregates.is_some(), "last entry should be the aggregate");
        let aggs = last.aggregates.as_ref().unwrap();
        assert_eq!(aggs.len(), 4);
        // Every aggregate target resolves via `lookup`.
        for target in aggs {
            assert!(lookup(target).is_some(), "aggregate target {target} must exist");
        }
    }

    #[test]
    fn lookup_returns_exact_match_only() {
        assert!(lookup("年报").is_some());
        assert!(lookup("定期报告").is_some());
        assert!(lookup("年").is_none()); // no prefix / fuzzy match
        assert!(lookup("annual").is_none()); // no English alias
        assert!(lookup("").is_none());
    }

    #[test]
    fn lookup_by_key_resolves_cninfo_column_id_to_zh() {
        let entry = lookup_by_key("category_ndbg_szsh").expect("年报 entry");
        assert_eq!(entry.zh, "年报");
        let entry = lookup_by_key("category_kzzq_szsh").expect("可转债 entry");
        assert_eq!(entry.zh, "可转债");
        // Aggregate entry has no `category` key so it never matches.
        assert!(lookup_by_key("definitely_not_a_real_key").is_none());
        assert!(lookup_by_key("").is_none());
    }

    #[test]
    fn validate_rejects_aggregate_pointing_at_missing_zh() {
        let bad = r#"[
            { "zh": "年报", "category": "category_ndbg_szsh" },
            { "zh": "定期报告", "aggregates": ["年报", "假报告"] }
        ]"#;
        let err = load_categories(bad).unwrap_err();
        assert!(err.contains("假报告"), "err: {err}");
        assert!(err.contains("not found"), "err: {err}");
    }

    #[test]
    fn validate_rejects_self_aggregating_entry() {
        let bad = r#"[
            { "zh": "年报", "category": "category_ndbg_szsh" },
            { "zh": "定期报告", "aggregates": ["定期报告"] }
        ]"#;
        assert!(load_categories(bad)
            .unwrap_err()
            .contains("aggregate itself"));
    }

    #[test]
    fn validate_rejects_entry_with_both_category_and_aggregates() {
        let bad = r#"[
            { "zh": "年报", "category": "x", "aggregates": ["y"] }
        ]"#;
        assert!(load_categories(bad).unwrap_err().contains("both"));
    }

    #[test]
    fn validate_rejects_entry_with_neither_field() {
        let bad = r#"[{ "zh": "年报" }]"#;
        assert!(load_categories(bad).unwrap_err().contains("neither"));
    }

    #[test]
    fn validate_rejects_duplicate_zh() {
        let bad = r#"[
            { "zh": "年报", "category": "x" },
            { "zh": "年报", "category": "y" }
        ]"#;
        assert!(load_categories(bad).unwrap_err().contains("duplicate"));
    }

    #[test]
    fn announcement_row_headers_match_readme_schema() {
        let h = AnnouncementRow::headers();
        assert_eq!(
            h,
            &[
                "id", "symbol", "name", "date", "type", "title", "format", "size_kb", "url",
                "source",
            ]
        );
    }

    #[test]
    fn announcement_row_cells_serialize_date_and_size() {
        let row = sample_row();
        let cells = row.cells();
        assert_eq!(cells.len(), 10);
        assert_eq!(cells[0], "1219506510");
        assert_eq!(cells[3], "2024-04-03");
        assert_eq!(cells[4], "年报");
        assert_eq!(cells[6], "pdf");
        assert_eq!(cells[7], "3481");
        assert_eq!(cells[9], "cninfo");
    }

    #[test]
    fn announcement_row_serializes_to_ndjson_with_type_rename() {
        let row = sample_row();
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["id"], "1219506510");
        assert_eq!(json["symbol"], "600519.SH");
        assert_eq!(json["date"], "2024-04-03");
        // `type_zh` field renamed to `type` (Rust keyword in user-facing form).
        assert_eq!(json["type"], "年报");
        assert!(json.get("type_zh").is_none());
        assert_eq!(json["format"], "pdf");
        assert_eq!(json["size_kb"], 3481);
        assert_eq!(json["source"], "cninfo");
    }
}
