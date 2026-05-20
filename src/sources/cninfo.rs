//! cninfo source — stock listings (F1 `search`) **and**
//! announcements query (F3 `announce list`).
//!
//! ## Stock listings (F1)
//! `szse_stock.json` / `hke_stock.json` → [`CnInfoRow`] /
//! [`StockLists`] / [`parse_envelope`]. Field semantics pinned in
//! the F1 README "数据源与协议".
//!
//! ## Announcements query (F3)
//! `POST /new/hisAnnouncement/query` form endpoint, returning a
//! paginated list of `RawAnnouncement` rows. [`query_announcements`]
//! handles:
//!
//! - resolving each user-supplied code to a `(code, orgId, market)`
//!   tuple via [`resolve_org_id`] (3-step: cache → auto-fetch → fail);
//! - splitting multi-market queries into `column=szse` vs
//!   `column=hke` POSTs;
//! - transparent pagination (`pageSize=30` server-side cap, walked
//!   with `pageNum=1,2,3,…`) honoring the user's `limit`;
//! - dedup across pages and across columns by `announcementId`;
//! - timestamp → Beijing (`+08:00`) date conversion that does not
//!   depend on the host's `TZ` environment.
//!
//! The F3 announcement API ships in Story 02; Story 03's
//! `sift announce list / show` and Story 04's `download` actually
//! consume it. The `#![allow(dead_code)]` below suppresses warnings
//! for the still-unused new surface until then.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use time::Date;

use crate::domain::announcement::{lookup_by_key, AnnouncementRow};
use crate::domain::market::Market;
use crate::error::SiftError;
use crate::http::HttpClient;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CnInfoRow {
    pub code: String,
    pub zwjc: String,
    pub pinyin: String,
    pub category: String,
    #[serde(rename = "orgId")]
    pub org_id: String,
}

#[derive(Debug)]
pub struct StockLists {
    /// Rows from `szse_stock.json` — covers SH / SZ / BJ / B-share / CDR
    /// despite the file name suggesting SZSE only.
    pub cn_a: Vec<CnInfoRow>,
    /// Rows from `hke_stock.json`.
    pub hk: Vec<CnInfoRow>,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "stockList")]
    stock_list: Vec<CnInfoRow>,
}

/// Decode a cninfo response body into its `stockList` array. `label`
/// gets embedded in the error message so a schema break can be traced
/// back to the originating endpoint without inspecting bytes.
pub(crate) fn parse_envelope(bytes: &[u8], label: &str) -> Result<Vec<CnInfoRow>, SiftError> {
    let env: Envelope = serde_json::from_slice(bytes).map_err(|e| {
        SiftError::Internal(format!("cninfo {label}: stockList missing ({e})"))
    })?;
    Ok(env.stock_list)
}

// ---------------------------------------------------------------------------
// F3 announcement query
// ---------------------------------------------------------------------------

const DEFAULT_BASE: &str = "http://www.cninfo.com.cn";
const ANNOUNCEMENT_PATH: &str = "/new/hisAnnouncement/query";
const PAGE_SIZE: u32 = 30;

/// Resolved base URL for cninfo. `SIFT_CNINFO_BASE` overrides the
/// default for tests (mockito) and any future integration harness.
pub(crate) fn cninfo_base() -> String {
    std::env::var("SIFT_CNINFO_BASE").unwrap_or_else(|_| DEFAULT_BASE.into())
}

/// User-supplied code paired with its cninfo `orgId` + inferred
/// market. Construct via [`resolve_org_id`] in the command layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSymbol {
    pub code: String,
    pub org_id: String,
    pub market: Market,
}

impl ResolvedSymbol {
    /// Exchange-suffixed display form: `600519.SH`, `00700.HK`.
    /// Derived from `org_id` prefix (cninfo's internal market tag).
    pub fn as_secucode(&self) -> String {
        let suffix = match self.market {
            Market::Hk => "HK",
            Market::Us => "US",
            Market::CnA => match self.org_id.as_str() {
                s if s.starts_with("gssh") => "SH",
                s if s.starts_with("gssz") => "SZ",
                s if s.starts_with("gfbj") || s.starts_with("gsbj") => "BJ",
                _ => "SH",
            },
        };
        format!("{}.{}", self.code, suffix)
    }
}

/// Input to [`query_announcements`]. `symbols` is the already
/// org-id-resolved list; `category` is a cninfo `category_*` key
/// (`定期报告` aggregate fan-out happens at the command layer);
/// `start` / `end` define an inclusive date range for the
/// `seDate=YYYY-MM-DD~YYYY-MM-DD` parameter. `limit` is the user
/// cap — the dispatcher walks as many pages as needed (or stops
/// early when `hasMore=false`).
#[derive(Debug, Clone)]
pub struct AnnouncementQuery {
    pub symbols: Vec<ResolvedSymbol>,
    pub category: Option<String>,
    pub keyword: Option<String>,
    pub start: Option<Date>,
    pub end: Option<Date>,
    pub limit: u32,
}

/// Resolve a single user-supplied code to a `ResolvedSymbol`.
///
/// Three-step strategy:
/// 1. Read the on-disk F1 cache via
///    [`crate::cache::search::load_cached_org_ids`] — if the code is
///    present, return immediately (zero network).
/// 2. Cache miss → call
///    [`crate::cache::search::fetch_stock_lists`] with default opts
///    (24h TTL, stale fallback). cninfo's name index is small + fast
///    so this is cheaper than refusing and asking the user to run
///    `sift search` first.
/// 3. Re-read the cache. Still missing → `SiftError::MissingOrgId`,
///    which exits 1 with a hint pointing at the code itself.
pub fn resolve_org_id(http: &HttpClient, code: &str) -> Result<ResolvedSymbol, SiftError> {
    use crate::cache::search::{fetch_stock_lists, load_cached_org_ids, SearchCacheOpts};
    if let Some((market, org_id)) = load_cached_org_ids().get(code) {
        return Ok(ResolvedSymbol {
            code: code.into(),
            org_id: org_id.clone(),
            market: *market,
        });
    }
    let _ = fetch_stock_lists(http, SearchCacheOpts::default())?;
    if let Some((market, org_id)) = load_cached_org_ids().get(code) {
        return Ok(ResolvedSymbol {
            code: code.into(),
            org_id: org_id.clone(),
            market: *market,
        });
    }
    Err(SiftError::MissingOrgId(code.into()))
}

/// Production entry: dispatch a cninfo announcement query against the
/// env-resolved base URL and return paginated, deduped, sorted rows.
pub fn query_announcements(
    http: &HttpClient,
    q: AnnouncementQuery,
) -> Result<Vec<AnnouncementRow>, SiftError> {
    let base = cninfo_base();
    query_announcements_with_base(http, q, &base)
}

/// Test seam for [`query_announcements`]. Production callers don't
/// need this — Story 02's unit tests point at a mockito URL through
/// it.
pub(crate) fn query_announcements_with_base(
    http: &HttpClient,
    q: AnnouncementQuery,
    base_url: &str,
) -> Result<Vec<AnnouncementRow>, SiftError> {
    if q.limit == 0 {
        return Ok(Vec::new());
    }
    let url = format!("{base_url}{ANNOUNCEMENT_PATH}");
    let by_code: HashMap<&str, &ResolvedSymbol> =
        q.symbols.iter().map(|s| (s.code.as_str(), s)).collect();

    let mut all: Vec<AnnouncementRow> = Vec::with_capacity(q.limit as usize);
    let mut seen: HashSet<String> = HashSet::new();
    for (column, syms) in split_by_column(&q.symbols) {
        if (all.len() as u32) >= q.limit {
            break;
        }
        let stock_param = syms
            .iter()
            .map(|s| format!("{},{}", s.code, s.org_id))
            .collect::<Vec<_>>()
            .join(";");
        let column_rows =
            fetch_paged(http, &url, column, &stock_param, &q, &by_code, &mut seen)?;
        all.extend(column_rows);
    }
    // Cross-page / cross-column ordering: newest first by date,
    // tiebreak by id desc (cninfo IDs are monotonically increasing).
    all.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.id.cmp(&a.id)));
    all.truncate(q.limit as usize);
    Ok(all)
}

/// Partition the symbol set by cninfo `column`. A-share (sh/sz/bj)
/// all share `column=szse`; HK goes to `column=hke`. Returns one
/// group per non-empty bucket so the caller can issue one POST per
/// group.
fn split_by_column(
    symbols: &[ResolvedSymbol],
) -> Vec<(&'static str, Vec<&ResolvedSymbol>)> {
    let mut szse: Vec<&ResolvedSymbol> = Vec::new();
    let mut hke: Vec<&ResolvedSymbol> = Vec::new();
    for s in symbols {
        if s.org_id.starts_with("gshk") {
            hke.push(s);
        } else {
            szse.push(s);
        }
    }
    let mut groups: Vec<(&'static str, Vec<&ResolvedSymbol>)> = Vec::new();
    if !szse.is_empty() {
        groups.push(("szse", szse));
    }
    if !hke.is_empty() {
        groups.push(("hke", hke));
    }
    groups
}

/// Walk `pageNum=1,2,…` until either `limit` is hit, the upstream
/// signals `hasMore=false`, or a page produces zero new rows after
/// dedup (a degenerate-but-possible end-of-stream signal). Always
/// returns *some* rows on success, even if the page was partially
/// dropped by dedup.
fn fetch_paged(
    http: &HttpClient,
    url: &str,
    column: &str,
    stock_param: &str,
    q: &AnnouncementQuery,
    by_code: &HashMap<&str, &ResolvedSymbol>,
    seen: &mut HashSet<String>,
) -> Result<Vec<AnnouncementRow>, SiftError> {
    let mut out = Vec::new();
    for page in 1u32.. {
        if (out.len() as u32) >= q.limit {
            break;
        }
        let form = build_form(column, stock_param, q, page);
        let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let bytes = http.post_form(url, &pairs)?;
        let resp: RawResponse = serde_json::from_slice(&bytes).map_err(|e| {
            SiftError::Internal(format!("cninfo announcements: parse: {e}"))
        })?;
        let rows = parse_response(&resp, by_code)?;
        let mut accepted = 0usize;
        for row in rows {
            if seen.insert(row.id.clone()) {
                out.push(row);
                accepted += 1;
                if (out.len() as u32) >= q.limit {
                    break;
                }
            }
        }
        if !resp.has_more || accepted == 0 {
            break;
        }
    }
    Ok(out)
}

/// Build the form-urlencoded body for one POST. Owned strings here
/// because the `pageNum` / `seDate` values are computed per-call;
/// the static keys stay `&'static str`.
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
        let type_zh = lookup_by_key(&raw.column_id)
            .map(|c| c.zh.clone())
            .unwrap_or_else(|| raw.column_id.clone());
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

/// `announcementTime` is a 13-digit unix epoch in milliseconds. cninfo
/// timestamps are wall-clock Beijing (UTC+8, no DST), so we convert
/// via a fixed `+08:00` offset rather than the host's `TZ` — this
/// gives the same date worldwide and matches the issuer's filing day.
fn ms_to_beijing_date(ms: i64) -> Option<Date> {
    let seconds = ms / 1000;
    let utc = time::OffsetDateTime::from_unix_timestamp(seconds).ok()?;
    let offset = time::UtcOffset::from_hms(8, 0, 0).ok()?;
    Some(utc.to_offset(offset).date())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"stockList":[{"code":"600519","zwjc":"贵州茅台","pinyin":"gzmt","category":"A股","orgId":"gssh0600519"}]}"#;

    #[test]
    fn parse_envelope_extracts_rows() {
        let rows = parse_envelope(SAMPLE.as_bytes(), "szse").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].code, "600519");
        assert_eq!(rows[0].zwjc, "贵州茅台");
        assert_eq!(rows[0].pinyin, "gzmt");
        assert_eq!(rows[0].category, "A股");
        assert_eq!(rows[0].org_id, "gssh0600519");
    }

    #[test]
    fn parse_envelope_missing_field_is_internal() {
        let err = parse_envelope(br#"{"foo":1}"#, "szse").unwrap_err();
        match err {
            SiftError::Internal(m) => {
                assert!(m.contains("cninfo szse"), "msg: {m}");
                assert!(m.contains("stockList"), "msg: {m}");
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn parse_envelope_empty_array_is_ok() {
        let rows = parse_envelope(br#"{"stockList":[]}"#, "hke").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn category_passes_through_unchanged() {
        let body =
            r#"{"stockList":[{"code":"900901","zwjc":"x","pinyin":"x","category":"B股","orgId":"x"}]}"#;
        let rows = parse_envelope(body.as_bytes(), "szse").unwrap();
        assert_eq!(rows[0].category, "B股");
    }

    // -----------------------------------------------------------------
    // F3 announcement query tests
    // -----------------------------------------------------------------

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
    fn resolved_symbol_as_secucode_per_org_id_prefix() {
        assert_eq!(maotai().as_secucode(), "600519.SH");
        assert_eq!(pingan_bank().as_secucode(), "000001.SZ");
        assert_eq!(tencent().as_secucode(), "00700.HK");
        let bj = ResolvedSymbol {
            code: "832000".into(),
            org_id: "gfbj0832000".into(),
            market: Market::CnA,
        };
        assert_eq!(bj.as_secucode(), "832000.BJ");
    }

    #[test]
    fn split_by_column_partitions_szse_and_hke() {
        let syms = vec![maotai(), tencent(), pingan_bank()];
        let groups = split_by_column(&syms);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "szse");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "hke");
        assert_eq!(groups[1].1.len(), 1);
    }

    #[test]
    fn split_by_column_omits_empty_buckets() {
        let only_hk = vec![tencent()];
        let groups = split_by_column(&only_hk);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "hke");
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
        let rows = query_announcements_with_base(
            &HttpClient::new(),
            sample_query(vec![maotai()], 30),
            &server.url(),
        )
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
        let rows = query_announcements_with_base(
            &HttpClient::new(),
            sample_query(vec![maotai(), tencent()], 30),
            &server.url(),
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        m_szse.assert();
        m_hke.assert();
    }

    #[test]
    fn transparent_pagination_walks_until_has_more_false() {
        let mut server = mockito::Server::new();
        // Page 1: 30 rows + hasMore=true.
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
        let rows = query_announcements_with_base(
            &HttpClient::new(),
            sample_query(vec![maotai()], 100),
            &server.url(),
        )
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
        let rows = query_announcements_with_base(
            &HttpClient::new(),
            sample_query(vec![maotai()], 10),
            &server.url(),
        )
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
        let rows = query_announcements_with_base(
            &HttpClient::new(),
            sample_query(vec![maotai(), tencent()], 30),
            &server.url(),
        )
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
        let rows = query_announcements_with_base(
            &HttpClient::new(),
            sample_query(vec![maotai()], 30),
            &server.url(),
        )
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
        let rows = query_announcements_with_base(
            &HttpClient::new(),
            sample_query(vec![maotai()], 30),
            &server.url(),
        )
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
        let err = query_announcements_with_base(
            &HttpClient::new(),
            sample_query(vec![maotai()], 30),
            &server.url(),
        )
        .unwrap_err();
        assert!(matches!(err, SiftError::Internal(_)), "got {err:?}");
        assert!(err.to_string().contains("cninfo announcements"));
    }

    // -----------------------------------------------------------------
    // org-id cache helpers
    // -----------------------------------------------------------------

    #[test]
    fn load_cached_org_ids_reads_both_szse_and_hke_files() {
        use std::collections::HashMap;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let cninfo_dir = tmp.path().join("cninfo");
        std::fs::create_dir_all(&cninfo_dir).unwrap();
        std::fs::write(
            cninfo_dir.join("szse_stock.json"),
            r#"{"stockList":[{"code":"600519","zwjc":"贵州茅台","pinyin":"gzmt","category":"A股","orgId":"gssh0600519"}]}"#,
        )
        .unwrap();
        std::fs::write(
            cninfo_dir.join("hke_stock.json"),
            r#"{"stockList":[{"code":"00700","zwjc":"腾讯","pinyin":"tx","category":"港股","orgId":"gshk0000700"}]}"#,
        )
        .unwrap();
        let mut map: HashMap<String, (Market, String)> = HashMap::new();
        crate::cache::search::load_cached_org_ids_at(&cninfo_dir, &mut map);
        assert_eq!(map.len(), 2);
        assert_eq!(map["600519"], (Market::CnA, "gssh0600519".into()));
        assert_eq!(map["00700"], (Market::Hk, "gshk0000700".into()));
    }

    #[test]
    fn load_cached_org_ids_returns_empty_when_no_files() {
        use std::collections::HashMap;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let mut map: HashMap<String, (Market, String)> = HashMap::new();
        crate::cache::search::load_cached_org_ids_at(&tmp.path().join("cninfo"), &mut map);
        assert!(map.is_empty());
    }
}
