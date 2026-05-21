//! Input layer for `sift announce`: stdin parsing, clap value
//! parsers, dict / source lookups. Pure transforms — every function
//! turns raw user input or upstream-derived bytes into resolved
//! domain values. No cache writes, no rendering.

use std::collections::HashMap;
use std::io::BufRead;

use time::format_description::well_known::Iso8601;
use time::Date;

use crate::domain::announcement::{categories, lookup, AnnouncementRow};
use crate::domain::Symbol;
use crate::error::SiftError;
use crate::http::HttpClient;
use crate::sources::cninfo::{resolve_org_id, ResolvedSymbol};

// ---------------------------------------------------------------------------
// clap value parsers
// ---------------------------------------------------------------------------

/// Build the `--type` value parser at clap-Command construction time.
/// Using `PossibleValuesParser` gives us two things for free: the full
/// 27-entry list in `--help`, and a friendly `[possible values: …]`
/// footer on the rejection error (clap exit code 2).
pub(super) fn type_value_parser() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(
        categories()
            .iter()
            .map(|c| c.zh.as_str())
            .collect::<Vec<_>>(),
    )
}

/// `YYYY-MM-DD` parser shared by `--start` / `--end`. Returns the
/// error as a `String` so clap renders it inline with the offending
/// argument name (exit code 2).
pub(super) fn parse_iso_date(s: &str) -> Result<Date, String> {
    Date::parse(s, &Iso8601::DATE).map_err(|e| format!("invalid date {s:?}: {e}"))
}

// ---------------------------------------------------------------------------
// Dict-side resolution
// ---------------------------------------------------------------------------

/// Look up the cninfo `category_*` key for a 中文 type name. Returns
/// `None` for aggregate entries (they have no `category` field) and
/// for entries not in the dictionary; the caller is expected to have
/// pre-validated via [`type_value_parser`] so `None` here usually
/// signals an aggregate.
pub(super) fn lookup_category(zh: &str) -> Option<String> {
    lookup(zh).and_then(|c| c.category.clone())
}

/// Translate the user's `--type` (already PossibleValuesParser-validated)
/// into one or more cninfo `category_*` keys. An aggregate entry fans
/// out into its constituents; absent `--type` becomes a single empty
/// key meaning "no category filter".
pub(super) fn expand_categories(zh: Option<&str>) -> Result<Vec<String>, SiftError> {
    let Some(zh) = zh else {
        return Ok(vec![String::new()]);
    };
    // Validated by clap, so a None here would be a programmer error.
    let cat = lookup(zh).ok_or_else(|| {
        SiftError::Internal(format!("dictionary unexpectedly missing {zh:?}"))
    })?;
    if let Some(aggs) = &cat.aggregates {
        eprintln!("[info] {zh} = {} 次子查询", aggs.len());
        return aggs
            .iter()
            .map(|agg_zh| {
                lookup_category(agg_zh).ok_or_else(|| {
                    SiftError::Internal(format!("aggregate target {agg_zh:?} has no category"))
                })
            })
            .collect();
    }
    Ok(vec![cat.category.clone().unwrap_or_default()])
}

/// Resolve every raw user input to a `ResolvedSymbol` via the cninfo
/// search cache (3-step: cache → auto-fetch → MissingOrgId). Errors
/// out on the first unresolved code — the user almost certainly wants
/// to fix the typo before issuing the (possibly large) query.
pub(super) fn resolve_all(
    http: &HttpClient,
    raw: &[String],
) -> Result<Vec<ResolvedSymbol>, SiftError> {
    raw.iter()
        .map(|u| {
            let sym = Symbol::parse(u)?;
            resolve_org_id(http, &sym.code)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Stdin parsing
// ---------------------------------------------------------------------------

/// Reconstruct an [`AnnouncementRow`] from a single NDJSON object.
/// Thin wrapper around `serde_json::from_value` — the heavy lifting
/// (ISO date parsing, forcing `format`/`source` to canonical
/// `&'static str` literals via `skip_deserializing`) lives in the
/// derived `Deserialize` impl on the struct. A missing required field
/// surfaces as `SiftError::Internal` so the user sees which key the
/// piped row was missing.
fn parse_announcement_value(v: &serde_json::Value) -> Result<AnnouncementRow, SiftError> {
    serde_json::from_value(v.clone())
        .map_err(|e| SiftError::Internal(format!("stdin row deserialize: {e}")))
}

/// Parse `stdin` as NDJSON and return the first row whose `id` field
/// matches `id`. Malformed lines are silently skipped — the producer
/// of the pipe (sift list) only ever emits well-formed NDJSON, but a
/// stray non-JSON line (a `jq` filter artefact, an `[info]` log) must
/// not abort the search.
pub(super) fn read_stdin_for_id<R: BufRead>(
    reader: R,
    id: &str,
) -> Result<AnnouncementRow, SiftError> {
    for line in reader.lines() {
        let line = line.map_err(|e| SiftError::Internal(format!("stdin read: {e}")))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value): Result<serde_json::Value, _> = serde_json::from_str(trimmed) else {
            continue;
        };
        if value.get("id").and_then(|v| v.as_str()) == Some(id) {
            return parse_announcement_value(&value);
        }
    }
    Err(SiftError::NotFound(id.into()))
}

/// What [`read_stdin_url_map`] surfaces. `map` is the primary product
/// (id→url for the `download` command's URL context). `rows` carries
/// any line that also deserialized cleanly into a full
/// [`AnnouncementRow`] — the caller can persist those as a side
/// product so a follow-up `show <id>` is zero-network. Pushing this
/// out of the parser (instead of having stdin code write to cache
/// directly) keeps the input layer free of side effects.
pub(super) struct StdinUrls {
    pub map: HashMap<String, String>,
    pub rows: Vec<AnnouncementRow>,
}

/// Drain stdin NDJSON into an `id → url` lookup. Lines that fail to
/// parse, lack `id` / `url`, or carry an empty `url` are silently
/// skipped — keeping `download` resilient to logs / blank lines /
/// `jq` projections that strip fields. Any line that *also* shapes a
/// complete `AnnouncementRow` is accumulated in `rows` so the caller
/// can decide whether to cache it.
pub(super) fn read_stdin_url_map<R: BufRead>(reader: R) -> Result<StdinUrls, SiftError> {
    let mut map = HashMap::new();
    let mut rows: Vec<AnnouncementRow> = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| SiftError::Internal(format!("stdin read: {e}")))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v): Result<serde_json::Value, _> = serde_json::from_str(trimmed) else {
            continue;
        };
        let id = v.get("id").and_then(|x| x.as_str());
        let url = v.get("url").and_then(|x| x.as_str());
        if let (Some(id), Some(url)) = (id, url) {
            if !url.is_empty() {
                map.insert(id.to_string(), url.to_string());
            }
        }
        if let Ok(row) = parse_announcement_value(&v) {
            rows.push(row);
        }
    }
    Ok(StdinUrls { map, rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    fn d(y: i32, m: u8, day: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap()
    }

    #[test]
    fn parse_iso_date_accepts_yyyy_mm_dd() {
        let got = parse_iso_date("2024-04-03").unwrap();
        assert_eq!(got, d(2024, 4, 3));
    }

    #[test]
    fn parse_iso_date_rejects_garbage() {
        let err = parse_iso_date("not-a-date").unwrap_err();
        assert!(err.contains("invalid date"), "err: {err}");
    }

    #[test]
    fn lookup_category_resolves_known_and_returns_none_for_unknown() {
        assert_eq!(
            lookup_category("年报").as_deref(),
            Some("category_ndbg_szsh")
        );
        assert!(lookup_category("不存在").is_none());
        // Aggregate entries have no `category` key — also `None`.
        assert!(lookup_category("定期报告").is_none());
    }

    #[test]
    fn expand_categories_none_yields_single_empty_no_filter() {
        assert_eq!(expand_categories(None).unwrap(), vec![String::new()]);
    }

    #[test]
    fn expand_categories_known_returns_single_key() {
        assert_eq!(
            expand_categories(Some("年报")).unwrap(),
            vec!["category_ndbg_szsh".to_string()]
        );
    }

    #[test]
    fn expand_categories_aggregate_fans_out_to_four_keys_in_readme_order() {
        let keys = expand_categories(Some("定期报告")).unwrap();
        assert_eq!(
            keys,
            vec![
                "category_ndbg_szsh".to_string(),  // 年报
                "category_bndbg_szsh".to_string(), // 半年报
                "category_yjdbg_szsh".to_string(), // 一季报
                "category_sjdbg_szsh".to_string(), // 三季报
            ]
        );
    }

    #[test]
    fn read_stdin_for_id_finds_matching_id_among_many() {
        let stdin = r#"{"id":"100","symbol":"600519.SH","name":"x","date":"2024-04-03","type":"年报","title":"t","format":"pdf","size_kb":12,"url":"http://u","source":"cninfo"}
{"id":"101","symbol":"600519.SH","name":"y","date":"2024-04-04","type":"年报","title":"t2","format":"pdf","size_kb":34,"url":"http://v","source":"cninfo"}
"#;
        let row = read_stdin_for_id(stdin.as_bytes(), "101").unwrap();
        assert_eq!(row.id, "101");
        assert_eq!(row.date, d(2024, 4, 4));
        assert_eq!(row.size_kb, 34);
    }

    #[test]
    fn read_stdin_for_id_returns_not_found_when_no_match() {
        let stdin = r#"{"id":"100","symbol":"x","name":"x","date":"2024-04-03","type":"年报","title":"t","format":"pdf","size_kb":0,"url":"","source":"cninfo"}
"#;
        let err = read_stdin_for_id(stdin.as_bytes(), "999").unwrap_err();
        assert!(matches!(err, SiftError::NotFound(_)));
        assert_eq!(err.exit_code(), 4);
    }

    #[test]
    fn read_stdin_for_id_skips_malformed_lines() {
        let stdin = "not json\n{\"id\":\"100\",\"symbol\":\"x\",\"name\":\"x\",\"date\":\"2024-04-03\",\"type\":\"年报\",\"title\":\"t\",\"format\":\"pdf\",\"size_kb\":0,\"url\":\"\",\"source\":\"cninfo\"}\n";
        let row = read_stdin_for_id(stdin.as_bytes(), "100").unwrap();
        assert_eq!(row.id, "100");
    }

    #[test]
    fn read_stdin_url_map_collects_id_to_url_pairs() {
        let stdin = r#"{"id":"100","url":"http://x/100.PDF","title":"t"}
{"id":"101","url":"http://x/101.PDF"}
"#;
        let urls = read_stdin_url_map(stdin.as_bytes()).unwrap();
        assert_eq!(urls.map.len(), 2);
        assert_eq!(urls.map["100"], "http://x/100.PDF");
        assert_eq!(urls.map["101"], "http://x/101.PDF");
        // Neither line carried the full AnnouncementRow shape (missing
        // symbol/name/date/...), so no rows accumulate.
        assert!(urls.rows.is_empty());
    }

    #[test]
    fn read_stdin_url_map_skips_rows_missing_url_or_id_or_malformed() {
        let stdin = "not json\n{\"id\":\"only-id\"}\n{\"url\":\"http://x/orphan.PDF\"}\n{\"id\":\"empty-url\",\"url\":\"\"}\n{\"id\":\"100\",\"url\":\"http://x/100.PDF\"}\n";
        let urls = read_stdin_url_map(stdin.as_bytes()).unwrap();
        assert_eq!(urls.map.len(), 1);
        assert_eq!(urls.map["100"], "http://x/100.PDF");
    }

    #[test]
    fn read_stdin_url_map_extracts_full_rows_when_present() {
        let stdin = r#"{"id":"100","symbol":"600519.SH","name":"x","date":"2024-04-03","type":"年报","title":"t","format":"pdf","size_kb":12,"url":"http://u","source":"cninfo"}
{"id":"only-url","url":"http://y","title":"z"}
"#;
        let urls = read_stdin_url_map(stdin.as_bytes()).unwrap();
        assert_eq!(urls.map.len(), 2);
        // Only the first line deserialized into a full AnnouncementRow.
        assert_eq!(urls.rows.len(), 1);
        assert_eq!(urls.rows[0].id, "100");
        assert_eq!(urls.rows[0].symbol, "600519.SH");
    }
}
