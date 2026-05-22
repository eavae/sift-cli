//! cninfo announcements query: `POST /new/hisAnnouncement/query` form
//! endpoint, returning a paginated list of `RawAnnouncement` rows.
//!
//! The single entry is [`Announcements`] — a `struct + new() +
//! with_url()` adapter matching the project-wide source convention
//! (see [`crate::sources::eastmoney_financials::EastmoneyFinancialSource`] /
//! [`crate::sources::sina_financials::SinaFinancialSource`]). Two
//! methods cover the surface:
//!
//! - [`Announcements::query`] — fetch up to `q.limit` rows for the
//!   given filter, deduped across pages + columns, sorted newest-first.
//! - [`Announcements::paginate_market`] — streaming whole-market walk
//!   for the `announce show <id>` fallback; the caller's `on_page`
//!   callback decides when to stop.
//!
//! Both methods share a single internal pagination loop
//! ([`Announcements::paginate_pages`]) so the per-page transport +
//! dedup logic exists in exactly one place.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use time::Date;

use super::{cninfo_base, ResolvedSymbol, ANNOUNCEMENT_PATH, PAGE_SIZE};
use crate::domain::announcement::{lookup_by_key, AnnouncementRow};
use crate::error::SiftError;
use crate::http::HttpClient;

// ===========================================================================
// Public API
// ===========================================================================

/// Filter + bound for an [`Announcements::query`] call. `symbols` is
/// the already-org-id-resolved list; `category` is a cninfo
/// `category_*` key (`定期报告` aggregate fan-out happens at the
/// command layer); `start` / `end` define an inclusive date range
/// for the `seDate=YYYY-MM-DD~YYYY-MM-DD` parameter. `limit` is the
/// user cap — the dispatcher walks as many pages as needed (or
/// stops early when `hasMore=false`).
#[derive(Debug, Clone)]
pub struct AnnouncementQuery {
    pub symbols: Vec<ResolvedSymbol>,
    pub category: Option<String>,
    pub keyword: Option<String>,
    pub start: Option<Date>,
    pub end: Option<Date>,
    pub limit: u32,
}

impl AnnouncementQuery {
    /// The "whole-market scan" preset used by
    /// [`Announcements::paginate_market`]: no symbol restriction, no
    /// filter, and an unbounded `limit` (the streaming callback
    /// decides when to stop, not this struct).
    pub(crate) fn scan_all() -> Self {
        Self {
            symbols: Vec::new(),
            category: None,
            keyword: None,
            start: None,
            end: None,
            limit: u32::MAX,
        }
    }
}

/// cninfo announcements API client. Holds only the base URL — the
/// `HttpClient` is passed in at each call so the struct stays
/// trivially `Send + Sync` and can be reused across threads.
///
/// Production constructor [`Announcements::new`] reads
/// `SIFT_CNINFO_BASE` (test override) or falls back to the cninfo
/// default; test-only [`Announcements::with_url`] (gated by
/// `#[cfg(test)]`) takes the URL explicitly so mockito tests can
/// point at their per-test server.
pub struct Announcements {
    base_url: String,
}

impl Announcements {
    /// Production constructor. Reads `SIFT_CNINFO_BASE` or falls back
    /// to the cninfo default.
    pub fn new() -> Self {
        Self {
            base_url: cninfo_base(),
        }
    }

    /// Test-only seam — construct against an explicit URL. Production
    /// callers use [`Self::new`]. `#[cfg(test)]` keeps the symbol out
    /// of release binaries and out of dead-code lints.
    #[cfg(test)]
    pub fn with_url(url: impl Into<String>) -> Self {
        Self {
            base_url: url.into(),
        }
    }

    /// Fetch up to `q.limit` rows for the given query. Walks pages
    /// transparently, dedupes by `announcementId` across pages and
    /// across `column=szse` / `column=hke`, and returns the result
    /// sorted newest-first (tiebreak: id desc).
    pub fn query(
        &self,
        http: &HttpClient,
        q: AnnouncementQuery,
    ) -> Result<Vec<AnnouncementRow>, SiftError> {
        if q.limit == 0 {
            return Ok(Vec::new());
        }
        let url = self.endpoint_url();
        let by_code: HashMap<&str, &ResolvedSymbol> =
            q.symbols.iter().map(|s| (s.code.as_str(), s)).collect();
        let groups = column_groups(&q.symbols);

        let mut all: Vec<AnnouncementRow> = Vec::with_capacity(q.limit as usize);
        let mut seen: HashSet<String> = HashSet::new();
        for (column, stock_param) in &groups {
            if (all.len() as u32) >= q.limit {
                break;
            }
            let limit = q.limit;
            let req = PageRequest {
                http,
                url: &url,
                column,
                stock_param,
                query: &q,
                by_code: &by_code,
            };
            self.paginate_pages(&req, &mut seen, &mut |fresh| {
                for row in fresh {
                    if (all.len() as u32) >= limit {
                        return ControlFlow::Break(());
                    }
                    all.push(row.clone());
                }
                ControlFlow::Continue(())
            })?;
        }
        // Cross-page / cross-column ordering: newest first by date,
        // tiebreak by id desc (cninfo IDs are monotonically increasing).
        all.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.id.cmp(&a.id)));
        all.truncate(q.limit as usize);
        Ok(all)
    }

    /// Streaming whole-market scan. Walks both `szse` and `hke` columns
    /// newest-first, calling `on_page` with every page's deduped rows.
    /// Stops as soon as `on_page` returns `ControlFlow::Break(())`,
    /// when cninfo signals `hasMore=false`, or when a page contributes
    /// zero new rows after dedup.
    ///
    /// Designed for the `sift announce show <id>` fallback: walk until
    /// the target id surfaces. The caller is responsible for any side
    /// effect (caching, progress reporting) inside `on_page`; this
    /// method only owns transport + dedup.
    ///
    /// Returns the total number of new rows surfaced across all pages,
    /// regardless of how the loop terminated.
    pub fn paginate_market<F>(
        &self,
        http: &HttpClient,
        mut on_page: F,
    ) -> Result<usize, SiftError>
    where
        F: FnMut(&[AnnouncementRow]) -> ControlFlow<()>,
    {
        let url = self.endpoint_url();
        // Empty `by_code` = the caller did not pre-resolve symbols;
        // the parser falls back to whatever `secCode` / `secName`
        // cninfo returns in each row.
        let by_code: HashMap<&str, &ResolvedSymbol> = HashMap::new();
        let scan_query = AnnouncementQuery::scan_all();
        let mut seen: HashSet<String> = HashSet::new();
        let mut total = 0usize;
        for column in ["szse", "hke"] {
            let req = PageRequest {
                http,
                url: &url,
                column,
                stock_param: "",
                query: &scan_query,
                by_code: &by_code,
            };
            self.paginate_pages(&req, &mut seen, &mut |fresh| {
                total += fresh.len();
                on_page(fresh)
            })?;
        }
        Ok(total)
    }

    // -----------------------------------------------------------------------
    // Internal pagination (shared between query + paginate_market)
    // -----------------------------------------------------------------------

    /// Walk `pageNum=1,2,…` for one column until `on_page` Breaks,
    /// cninfo signals `hasMore=false`, or dedup leaves a page empty.
    /// `seen` is owned by the caller so dedup state composes across
    /// multiple columns.
    fn paginate_pages<F>(
        &self,
        req: &PageRequest,
        seen: &mut HashSet<String>,
        on_page: &mut F,
    ) -> Result<(), SiftError>
    where
        F: FnMut(&[AnnouncementRow]) -> ControlFlow<()>,
    {
        for page in 1u32.. {
            let (rows, has_more) = self.fetch_page(req, page)?;
            let mut fresh: Vec<AnnouncementRow> = Vec::with_capacity(rows.len());
            for row in rows {
                if seen.insert(row.id.clone()) {
                    fresh.push(row);
                }
            }
            let stopped = matches!(on_page(&fresh), ControlFlow::Break(()));
            if stopped || !has_more || fresh.is_empty() {
                break;
            }
        }
        Ok(())
    }

    /// One POST + decode + projection to `Vec<AnnouncementRow>`.
    /// Returns the parsed rows together with the upstream `hasMore`
    /// flag so the caller decides whether another page is worth
    /// requesting. No dedup, no callback — that lives in
    /// [`Self::paginate_pages`].
    fn fetch_page(
        &self,
        req: &PageRequest,
        page: u32,
    ) -> Result<(Vec<AnnouncementRow>, bool), SiftError> {
        let form = build_form(req.column, req.stock_param, req.query, page);
        let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let bytes = req.http.post_form(req.url, &pairs)?;
        let resp: RawResponse = serde_json::from_slice(&bytes)
            .map_err(|e| SiftError::Internal(format!("cninfo announcements: parse: {e}")))?;
        let rows = parse_response(&resp, req.by_code)?;
        Ok((rows, resp.has_more))
    }

    fn endpoint_url(&self) -> String {
        format!("{}{}", self.base_url, ANNOUNCEMENT_PATH)
    }
}

/// Per-column page-request context bundled together so the inner
/// pagination methods (`paginate_pages`, `fetch_page`) take a single
/// `&PageRequest` instead of seven individual borrows. Lifetimes are
/// all driven by the outer [`Announcements::query`] /
/// [`Announcements::paginate_market`] frame.
struct PageRequest<'a> {
    http: &'a HttpClient,
    url: &'a str,
    column: &'a str,
    stock_param: &'a str,
    query: &'a AnnouncementQuery,
    by_code: &'a HashMap<&'a str, &'a ResolvedSymbol>,
}

impl Default for Announcements {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Request planning
// ===========================================================================

/// Plan the cninfo POSTs for a [`Announcements::query`] call: one
/// `(column, stock_param)` per non-empty bucket. A-share symbols
/// (`gssh*` / `gssz*` / `gfbj*` / `gsbj*`) all share `column=szse`;
/// HK (`gshk*`) goes to `column=hke`. An empty `symbols` slice
/// expands to both columns with empty stock parameters — cninfo
/// accepts that as a whole-market scan when a date window is
/// supplied.
fn column_groups(symbols: &[ResolvedSymbol]) -> Vec<(&'static str, String)> {
    if symbols.is_empty() {
        return vec![("szse", String::new()), ("hke", String::new())];
    }
    let (hke, szse): (Vec<&ResolvedSymbol>, Vec<&ResolvedSymbol>) =
        symbols.iter().partition(|s| s.org_id.starts_with("gshk"));
    let mut groups: Vec<(&'static str, String)> = Vec::new();
    if !szse.is_empty() {
        groups.push(("szse", join_stock(&szse)));
    }
    if !hke.is_empty() {
        groups.push(("hke", join_stock(&hke)));
    }
    groups
}

/// `code,orgId;code,orgId;…` — the form-encoded `stock` value.
fn join_stock(symbols: &[&ResolvedSymbol]) -> String {
    symbols
        .iter()
        .map(|s| format!("{},{}", s.code, s.org_id))
        .collect::<Vec<_>>()
        .join(";")
}

// ===========================================================================
// Wire (form construction + response decode)
// ===========================================================================

/// Build the form-urlencoded body for one POST. Owned strings here
/// because the `pageNum` / `seDate` values are computed per-call; the
/// static keys stay `&'static str`.
fn build_form(
    column: &str,
    stock_param: &str,
    q: &AnnouncementQuery,
    page: u32,
) -> Vec<(&'static str, String)> {
    let mut form: Vec<(&'static str, String)> = vec![
        ("stock", stock_param.into()),
        ("column", column.into()),
        ("pageNum", page.to_string()),
        ("pageSize", PAGE_SIZE.to_string()),
        ("tabName", "fulltext".into()),
    ];
    if let Some(cat) = &q.category {
        form.push(("category", cat.clone()));
    }
    if let Some(kw) = &q.keyword {
        form.push(("searchkey", kw.clone()));
    }
    if q.start.is_some() || q.end.is_some() {
        let start_s = q.start.map(date_to_iso).unwrap_or_default();
        let end_s = q.end.map(date_to_iso).unwrap_or_default();
        form.push(("seDate", format!("{start_s}~{end_s}")));
    }
    form
}

fn date_to_iso(d: Date) -> String {
    d.format(&time::format_description::well_known::Iso8601::DATE)
        .unwrap_or_default()
}

#[derive(serde::Deserialize, Default)]
struct RawResponse {
    #[serde(rename = "announcements", default)]
    items: Option<Vec<RawAnnouncement>>,
    #[serde(rename = "hasMore", default)]
    has_more: bool,
}

#[derive(serde::Deserialize)]
struct RawAnnouncement {
    #[serde(rename = "announcementId")]
    id: String,
    #[serde(rename = "secCode")]
    code: String,
    #[serde(rename = "secName")]
    name: String,
    #[serde(rename = "announcementTime")]
    time_ms: i64,
    #[serde(rename = "announcementTitle")]
    title: String,
    #[serde(rename = "adjunctUrl")]
    adjunct_url: String,
    #[serde(rename = "adjunctSize", default)]
    adjunct_size_kb: u32,
    #[serde(rename = "columnId", default)]
    column_id: String,
}

fn parse_response(
    resp: &RawResponse,
    by_code: &HashMap<&str, &ResolvedSymbol>,
) -> Result<Vec<AnnouncementRow>, SiftError> {
    let Some(items) = &resp.items else {
        return Ok(Vec::new()); // `announcements: null` = no results
    };
    let mut out = Vec::with_capacity(items.len());
    for raw in items {
        // A row missing a usable timestamp is dropped rather than
        // emitting `1970-01-01` — better to undercount than mislead.
        let Some(date) = ms_to_beijing_date(raw.time_ms) else {
            continue;
        };
        let symbol_str = match by_code.get(raw.code.as_str()) {
            Some(r) => r.as_secucode(),
            None => raw.code.clone(),
        };
        // cninfo's `columnId` may carry multiple categories separated
        // by `||` (e.g. `250401||251302` for HK 公告). Translate each
        // segment via the dictionary and rejoin with a single `|` so
        // the user-facing form is one token per category — narrower,
        // easier to read in a table, and still trivially splittable.
        let type_zh = raw
            .column_id
            .split("||")
            .map(|key| {
                lookup_by_key(key)
                    .map(|c| c.zh.clone())
                    .unwrap_or_else(|| key.to_string())
            })
            .collect::<Vec<_>>()
            .join("|");
        let url = if raw.adjunct_url.is_empty() {
            String::new()
        } else {
            format!("http://static.cninfo.com.cn/{}", raw.adjunct_url)
        };
        out.push(AnnouncementRow {
            id: raw.id.clone(),
            symbol: symbol_str,
            name: raw.name.clone(),
            date,
            type_zh,
            title: raw.title.clone(),
            format: "pdf",
            size_kb: raw.adjunct_size_kb,
            url,
            source: "cninfo",
        });
    }
    Ok(out)
}

/// `announcementTime` is a 13-digit unix epoch in milliseconds.
/// cninfo timestamps are wall-clock Beijing (UTC+8, no DST), so we
/// convert via a fixed `+08:00` offset rather than the host's `TZ`
/// — this gives the same date worldwide and matches the issuer's
/// filing day.
fn ms_to_beijing_date(ms: i64) -> Option<Date> {
    let seconds = ms / 1000;
    let utc = time::OffsetDateTime::from_unix_timestamp(seconds).ok()?;
    let offset = time::UtcOffset::from_hms(8, 0, 0).ok()?;
    Some(utc.to_offset(offset).date())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::Market;
    use time::Month;

    fn d(y: i32, m: u8, day: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap()
    }

    fn maotai() -> ResolvedSymbol {
        ResolvedSymbol {
            code: "600519".into(),
            org_id: "gssh0600519".into(),
            market: Market::CnA,
        }
    }
    fn tencent() -> ResolvedSymbol {
        ResolvedSymbol {
            code: "00700".into(),
            org_id: "gshk0000700".into(),
            market: Market::Hk,
        }
    }
    fn pingan_bank() -> ResolvedSymbol {
        ResolvedSymbol {
            code: "000001".into(),
            org_id: "gssz0000001".into(),
            market: Market::CnA,
        }
    }

    #[test]
    fn ms_to_beijing_date_uses_fixed_offset() {
        // 2024-04-03 00:00:00 Beijing = 2024-04-02 16:00:00 UTC.
        let ms: i64 = 1_712_073_600_000;
        assert_eq!(ms_to_beijing_date(ms).unwrap(), d(2024, 4, 3));
        // 2024-04-03 23:59:59.999 Beijing = 1712159999999.
        let near_midnight = ms_to_beijing_date(1_712_159_999_999).unwrap();
        assert_eq!(near_midnight, d(2024, 4, 3));
        // 2024-04-04 00:00:00 Beijing = 1712160000000.
        assert_eq!(
            ms_to_beijing_date(1_712_160_000_000).unwrap(),
            d(2024, 4, 4)
        );
    }

    #[test]
    fn column_groups_partitions_szse_and_hke_with_stock_param() {
        let syms = vec![maotai(), tencent(), pingan_bank()];
        let groups = column_groups(&syms);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "szse");
        assert_eq!(groups[0].1, "600519,gssh0600519;000001,gssz0000001");
        assert_eq!(groups[1].0, "hke");
        assert_eq!(groups[1].1, "00700,gshk0000700");
    }

    #[test]
    fn column_groups_omits_empty_buckets() {
        let only_hk = vec![tencent()];
        let groups = column_groups(&only_hk);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "hke");
    }

    #[test]
    fn column_groups_empty_input_expands_to_both_with_blank_stock() {
        let groups = column_groups(&[]);
        assert_eq!(
            groups,
            vec![("szse", String::new()), ("hke", String::new())]
        );
    }

    fn sample_query(symbols: Vec<ResolvedSymbol>, limit: u32) -> AnnouncementQuery {
        AnnouncementQuery {
            symbols,
            category: Some("category_ndbg_szsh".into()),
            keyword: None,
            start: Some(d(2024, 1, 1)),
            end: Some(d(2024, 12, 31)),
            limit,
        }
    }

    #[test]
    fn build_form_includes_required_fields_and_se_date() {
        let q = sample_query(vec![maotai()], 50);
        let form = build_form("szse", "600519,gssh0600519", &q, 2);
        let map: HashMap<&str, String> = form.into_iter().collect();
        assert_eq!(map["stock"], "600519,gssh0600519");
        assert_eq!(map["column"], "szse");
        assert_eq!(map["pageNum"], "2");
        assert_eq!(map["pageSize"], "30");
        assert_eq!(map["tabName"], "fulltext");
        assert_eq!(map["category"], "category_ndbg_szsh");
        assert_eq!(map["seDate"], "2024-01-01~2024-12-31");
    }

    #[test]
    fn build_form_omits_optional_when_unset() {
        let q = AnnouncementQuery {
            symbols: vec![maotai()],
            category: None,
            keyword: None,
            start: None,
            end: None,
            limit: 10,
        };
        let form = build_form("szse", "600519,gssh0600519", &q, 1);
        let keys: Vec<&str> = form.iter().map(|(k, _)| *k).collect();
        assert!(!keys.contains(&"category"));
        assert!(!keys.contains(&"searchkey"));
        assert!(!keys.contains(&"seDate"));
    }

    // -- response parsing fixtures ------------------------------------

    fn ann_json(id: &str, code: &str, time_ms: i64, column: &str, title: &str) -> String {
        format!(
            r#"{{
                "announcementId":"{id}",
                "secCode":"{code}",
                "secName":"贵州茅台",
                "announcementTime":{time_ms},
                "announcementTitle":"{title}",
                "adjunctUrl":"finalpage/2024-04-03/{id}.PDF",
                "adjunctSize":3481,
                "columnId":"{column}"
            }}"#
        )
    }

    fn page_body(ids_times: &[(&str, i64)], has_more: bool) -> String {
        let arr = ids_times
            .iter()
            .map(|(id, ms)| ann_json(id, "600519", *ms, "category_ndbg_szsh", "annual"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"announcements":[{arr}],"hasMore":{has_more}}}"#,
            has_more = has_more
        )
    }

    fn empty_body() -> &'static str {
        r#"{"announcements":null,"hasMore":false}"#
    }

    fn page_mock(server: &mut mockito::ServerGuard, page: u32, body: String) -> mockito::Mock {
        server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::Regex(format!(
                r"(^|&)pageNum={page}(&|$)"
            )))
            .with_status(200)
            .with_body(body)
            .expect(1)
            .create()
    }

    /// Build a `RawResponse` with a single row having `column_id =
    /// raw_col` and run it through `parse_response` to get the final
    /// `type_zh`. Lets us pin down the `||` → `|` translation without
    /// spinning a mockito server.
    fn translate_column_id(raw_col: &str) -> String {
        let resp = RawResponse {
            items: Some(vec![RawAnnouncement {
                id: "1".into(),
                code: "600519".into(),
                name: "n".into(),
                time_ms: 1_712_160_000_000,
                title: "t".into(),
                adjunct_url: "x".into(),
                adjunct_size_kb: 0,
                column_id: raw_col.into(),
            }]),
            has_more: false,
        };
        let by_code: HashMap<&str, &ResolvedSymbol> = HashMap::new();
        parse_response(&resp, &by_code).unwrap()[0]
            .type_zh
            .clone()
    }

    #[test]
    fn column_id_single_known_translates_to_zh() {
        assert_eq!(translate_column_id("category_ndbg_szsh"), "年报");
    }

    #[test]
    fn column_id_single_unknown_passes_through_verbatim() {
        assert_eq!(translate_column_id("250401"), "250401");
    }

    #[test]
    fn column_id_double_pipe_splits_and_rejoins_with_single_pipe() {
        assert_eq!(translate_column_id("250401||251302"), "250401|251302");
    }

    #[test]
    fn column_id_double_pipe_mixed_known_and_unknown_translates_each_segment() {
        assert_eq!(
            translate_column_id("category_ndbg_szsh||251302"),
            "年报|251302"
        );
    }

    #[test]
    fn single_symbol_single_page_returns_translated_rows() {
        let mut server = mockito::Server::new();
        let body = page_body(&[("100", 1_712_073_600_000), ("101", 1_712_160_000_000)], false);
        let _m = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .with_status(200)
            .with_body(body)
            .expect(1)
            .create();
        let rows = Announcements::with_url(server.url())
            .query(&HttpClient::new(), sample_query(vec![maotai()], 30))
            .unwrap();
        assert_eq!(rows.len(), 2);
        // Sorted newest first; ID 101 (later timestamp) leads.
        assert_eq!(rows[0].id, "101");
        assert_eq!(rows[0].symbol, "600519.SH");
        assert_eq!(rows[0].type_zh, "年报");
        assert_eq!(rows[0].format, "pdf");
        assert_eq!(rows[0].source, "cninfo");
        assert!(rows[0]
            .url
            .starts_with("http://static.cninfo.com.cn/finalpage/"));
        assert_eq!(rows[0].size_kb, 3481);
    }

    #[test]
    fn cross_market_query_issues_two_posts_with_distinct_columns() {
        let mut server = mockito::Server::new();
        let m_szse = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::Regex(r"(^|&)column=szse(&|$)".into()))
            .with_status(200)
            .with_body(page_body(&[("S1", 1_712_160_000_000)], false))
            .expect(1)
            .create();
        let m_hke = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::Regex(r"(^|&)column=hke(&|$)".into()))
            .with_status(200)
            .with_body(page_body(&[("H1", 1_712_073_600_000)], false))
            .expect(1)
            .create();
        let rows = Announcements::with_url(server.url())
            .query(&HttpClient::new(), sample_query(vec![maotai(), tencent()], 30))
            .unwrap();
        assert_eq!(rows.len(), 2);
        m_szse.assert();
        m_hke.assert();
    }

    #[test]
    fn transparent_pagination_walks_until_has_more_false() {
        let mut server = mockito::Server::new();
        let p1: Vec<(&str, i64)> = (0..30)
            .map(|i| (Box::leak(format!("p1-{i}").into_boxed_str()) as &str, 1_712_073_600_000 - i as i64))
            .collect();
        let p2: Vec<(&str, i64)> = (0..30)
            .map(|i| (Box::leak(format!("p2-{i}").into_boxed_str()) as &str, 1_712_073_600_000 - 100 - i as i64))
            .collect();
        let p3: Vec<(&str, i64)> = (0..10)
            .map(|i| (Box::leak(format!("p3-{i}").into_boxed_str()) as &str, 1_712_073_600_000 - 200 - i as i64))
            .collect();
        let _m1 = page_mock(&mut server, 1, page_body(&p1, true));
        let _m2 = page_mock(&mut server, 2, page_body(&p2, true));
        let _m3 = page_mock(&mut server, 3, page_body(&p3, false));
        let rows = Announcements::with_url(server.url())
            .query(&HttpClient::new(), sample_query(vec![maotai()], 100))
            .unwrap();
        assert_eq!(rows.len(), 70);
    }

    #[test]
    fn limit_truncates_before_extra_page_is_fetched() {
        let mut server = mockito::Server::new();
        // Only page 1 should be requested. If `limit=10` somehow asked
        // for page 2, mockito (no second mock) returns 501 → the test
        // would surface a Network error.
        let p1: Vec<(&str, i64)> = (0..30)
            .map(|i| (Box::leak(format!("a{i}").into_boxed_str()) as &str, 1_712_073_600_000 - i as i64))
            .collect();
        let m = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .with_status(200)
            .with_body(page_body(&p1, true))
            .expect(1)
            .create();
        let rows = Announcements::with_url(server.url())
            .query(&HttpClient::new(), sample_query(vec![maotai()], 10))
            .unwrap();
        assert_eq!(rows.len(), 10);
        m.assert();
    }

    #[test]
    fn dedup_across_columns_keeps_first_occurrence() {
        let mut server = mockito::Server::new();
        // szse and hke both return the same announcementId "DUP".
        let _szse = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::Regex(r"(^|&)column=szse(&|$)".into()))
            .with_status(200)
            .with_body(page_body(&[("DUP", 1_712_160_000_000)], false))
            .expect(1)
            .create();
        let _hke = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::Regex(r"(^|&)column=hke(&|$)".into()))
            .with_status(200)
            .with_body(page_body(&[("DUP", 1_712_160_000_000)], false))
            .expect(1)
            .create();
        let rows = Announcements::with_url(server.url())
            .query(&HttpClient::new(), sample_query(vec![maotai(), tencent()], 30))
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "DUP");
    }

    #[test]
    fn empty_announcements_returns_ok_with_no_rows() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .with_status(200)
            .with_body(empty_body())
            .expect(1)
            .create();
        let rows = Announcements::with_url(server.url())
            .query(&HttpClient::new(), sample_query(vec![maotai()], 30))
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn missing_top_level_keys_parses_to_empty_rows_not_error() {
        // `RawResponse` has all-default fields so `{"foo":1}` gives
        // `items=None, has_more=false` — empty rows, not Err. This is
        // intentional: cninfo's "no announcements" response shape is
        // `{"announcements":null,...}`, which deserializes identically.
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .with_status(200)
            .with_body(r#"{"foo":1}"#)
            .expect(1)
            .create();
        let rows = Announcements::with_url(server.url())
            .query(&HttpClient::new(), sample_query(vec![maotai()], 30))
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn truly_malformed_json_is_internal_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .with_status(200)
            .with_body("not json")
            .expect(1)
            .create();
        let err = Announcements::with_url(server.url())
            .query(&HttpClient::new(), sample_query(vec![maotai()], 30))
            .unwrap_err();
        assert!(matches!(err, SiftError::Internal(_)), "got {err:?}");
        assert!(err.to_string().contains("cninfo announcements"));
    }

    // ------------------------------------------------------------------
    // Streaming paginate (announce show fallback)
    // ------------------------------------------------------------------

    #[test]
    fn paginate_streams_rows_and_breaks_on_target_id() {
        // Two pages on `szse`; target id is on page 2. Callback breaks
        // as soon as the id appears, so page 3 + the `hke` column
        // never get hit.
        let mut server = mockito::Server::new();
        let _p1 = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex(r"(^|&)column=szse(&|$)".into()),
                mockito::Matcher::Regex(r"(^|&)pageNum=1(&|$)".into()),
            ]))
            .with_status(200)
            .with_body(page_body(&[("X1", 1_712_160_000_000)], true))
            .expect(1)
            .create();
        let _p2 = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex(r"(^|&)column=szse(&|$)".into()),
                mockito::Matcher::Regex(r"(^|&)pageNum=2(&|$)".into()),
            ]))
            .with_status(200)
            .with_body(page_body(&[("TARGET", 1_712_073_600_000)], true))
            .expect(1)
            .create();
        // Page 3 mock exists but `expect(0)` — must never be hit
        // because the callback breaks on page 2.
        let _p3 = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex(r"(^|&)column=szse(&|$)".into()),
                mockito::Matcher::Regex(r"(^|&)pageNum=3(&|$)".into()),
            ]))
            .with_status(200)
            .with_body(empty_body())
            .expect(0)
            .create();
        let _hke = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::Regex(r"(^|&)column=hke(&|$)".into()))
            .with_status(200)
            .with_body(empty_body())
            .expect(0)
            .create();

        let mut seen: Vec<String> = Vec::new();
        let total = Announcements::with_url(server.url())
            .paginate_market(&HttpClient::new(), |page_rows| {
                for r in page_rows {
                    seen.push(r.id.clone());
                    if r.id == "TARGET" {
                        return ControlFlow::Break(());
                    }
                }
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(seen, vec!["X1", "TARGET"]);
        assert_eq!(total, 2);
    }

    #[test]
    fn paginate_walks_both_columns_when_callback_never_breaks() {
        // szse exhausts after page 1 (`has_more=false`), then hke is
        // hit once and also exhausts. Callback always continues.
        let mut server = mockito::Server::new();
        let _szse = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::Regex(r"(^|&)column=szse(&|$)".into()))
            .with_status(200)
            .with_body(page_body(&[("S1", 1_712_160_000_000)], false))
            .expect(1)
            .create();
        let _hke = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::Regex(r"(^|&)column=hke(&|$)".into()))
            .with_status(200)
            .with_body(page_body(&[("H1", 1_712_073_600_000)], false))
            .expect(1)
            .create();
        let mut count = 0usize;
        Announcements::with_url(server.url())
            .paginate_market(&HttpClient::new(), |page_rows| {
                count += page_rows.len();
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(count, 2, "saw both szse + hke rows");
    }

    #[test]
    fn paginate_dedupes_repeated_ids_across_pages() {
        // Two pages on szse where page 2 repeats page 1's id. The
        // dedup map filters the duplicate before the callback sees it.
        let mut server = mockito::Server::new();
        let _p1 = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex(r"(^|&)column=szse(&|$)".into()),
                mockito::Matcher::Regex(r"(^|&)pageNum=1(&|$)".into()),
            ]))
            .with_status(200)
            .with_body(page_body(&[("DUP", 1_712_160_000_000)], true))
            .expect(1)
            .create();
        let _p2 = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex(r"(^|&)column=szse(&|$)".into()),
                mockito::Matcher::Regex(r"(^|&)pageNum=2(&|$)".into()),
            ]))
            .with_status(200)
            .with_body(page_body(&[("DUP", 1_712_160_000_000)], false))
            .expect(1)
            .create();
        let _hke = server
            .mock("POST", ANNOUNCEMENT_PATH)
            .match_body(mockito::Matcher::Regex(r"(^|&)column=hke(&|$)".into()))
            .with_status(200)
            .with_body(empty_body())
            .expect(1)
            .create();
        let mut seen: Vec<String> = Vec::new();
        Announcements::with_url(server.url())
            .paginate_market(&HttpClient::new(), |page_rows| {
                for r in page_rows {
                    seen.push(r.id.clone());
                }
                ControlFlow::Continue(())
            })
            .unwrap();
        // "DUP" only emitted once; page 2's repeat is dropped pre-callback.
        assert_eq!(seen, vec!["DUP"]);
    }
}
