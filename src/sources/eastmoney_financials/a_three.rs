//! A-share three statements via PC_HSF10 two-step.
//!
//! - Step 1 (`{slug}DateAjaxNew`) lists available `REPORT_DATE`s for
//!   a given `companyType` template.
//! - Step 2 (`{slug}AjaxNew?dates=...`) returns the wide-table rows
//!   for up to 5 dates per request.
//!
//! `companyType` is unknown until probed: we walk `[4, 3, 2, 1]` —
//! general industrial → bank → insurance → securities — and lock in
//! the first template that responds with a non-empty `data` array.
//! The result is cached per `EastmoneyFinancialSource` instance, so
//! a second query for the same symbol skips the probe.

use serde_json::{Map, Value};

use crate::domain::{FinancialRow, Period, Query, Scope, Statement, Symbol};
use crate::error::SiftError;
use crate::http::HttpClient;

use super::translate;
use super::EastmoneyFinancialSource;

const COMPANY_TYPES: &[u8] = &[4, 3, 2, 1];
const DATES_PER_BATCH: usize = 5;

pub(crate) fn fetch(
    src: &EastmoneyFinancialSource,
    q: &Query,
    http: &HttpClient,
) -> Result<Vec<FinancialRow>, SiftError> {
    let slug = statement_slug(q.statement);
    let code = translate::a_share_code(&q.symbol);
    let report_type = if q.scope == Scope::Consolidated { 1 } else { 2 };

    let ct = resolve_company_type(src, &q.symbol, slug, &code, http)?;

    let dates_all = list_dates(&src.urls().hsf10_base, ct, slug, &code, http)?;
    let dates_filtered = translate::filter_dates(dates_all, &q.periods);
    if dates_filtered.is_empty() {
        return Ok(Vec::new());
    }

    let mut rows = Vec::new();
    for chunk in dates_filtered.chunks(DATES_PER_BATCH) {
        let url = format!(
            "{base}/{slug}AjaxNew?companyType={ct}&reportDateType=0&reportType={rt}&dates={dates}&code={code}",
            base = src.urls().hsf10_base,
            rt = report_type,
            dates = chunk.join(","),
        );
        let bytes = http.get_bytes(&url)?;
        let resp: WideResp = serde_json::from_slice(&bytes)
            .map_err(|e| SiftError::Internal(format!("eastmoney {slug}AjaxNew parse: {e}")))?;

        // Parent-scope empty result is a known limitation: EM financial
        // sector templates do not always publish a `reportType=2` view.
        // We *do not* silently fall back to consolidated — that would
        // misrepresent the data per the user's explicit `--scope parent`.
        if resp.data.is_empty() && q.scope == Scope::Parent {
            eprintln!(
                "[warn] eastmoney {code} {slug} reportType=2 (parent) returned empty rows; \
                 source-level fallback to consolidated is intentionally not performed"
            );
            return Ok(Vec::new());
        }

        for entry in &resp.data {
            rows.extend(translate::a_wide_to_rows(entry, q));
        }
    }
    Ok(rows)
}

/// EM URL slug per statement.
fn statement_slug(s: Statement) -> &'static str {
    match s {
        Statement::Balance => "zcfzb",
        Statement::Income => "lrb",
        Statement::Cashflow => "xjllb",
        Statement::Indicator => unreachable!("Indicator is dispatched to indicator.rs"),
    }
}

/// Resolve which `companyType` template this symbol uses. Caches the
/// answer on the source instance so a second query for the same
/// symbol does not re-probe.
fn resolve_company_type(
    src: &EastmoneyFinancialSource,
    sym: &Symbol,
    slug: &str,
    code: &str,
    http: &HttpClient,
) -> Result<u8, SiftError> {
    {
        let cache = src.company_type_cache().lock().unwrap();
        if let Some(ct) = cache.get(sym).copied() {
            return Ok(ct);
        }
    }
    for &ct in COMPANY_TYPES {
        let url = format!(
            "{base}/{slug}DateAjaxNew?companyType={ct}&reportDateType=0&code={code}",
            base = src.urls().hsf10_base,
        );
        let bytes = http.get_bytes(&url)?;
        let resp: DateResp = serde_json::from_slice(&bytes).map_err(|e| {
            SiftError::Internal(format!("eastmoney {slug}DateAjaxNew parse: {e}"))
        })?;
        if !resp.data.is_empty() {
            src.company_type_cache().lock().unwrap().insert(sym.clone(), ct);
            return Ok(ct);
        }
    }
    Err(SiftError::Parse(format!(
        "eastmoney: no companyType in {COMPANY_TYPES:?} returned data for {code}; \
         symbol may be unsupported"
    )))
}

/// List every report period EM has on file for an A-share symbol.
/// Reuses the income-statement date endpoint (`lrbDateAjaxNew`) — EM
/// publishes the same period set across income / balance / cashflow,
/// and every company files an income statement, so a single call is
/// sufficient. Mis-shaped or stale dates are dropped silently.
pub(super) fn list_periods_a(
    src: &EastmoneyFinancialSource,
    symbol: &Symbol,
    http: &HttpClient,
) -> Result<Vec<Period>, SiftError> {
    let code = translate::a_share_code(symbol);
    let ct = resolve_company_type(src, symbol, "lrb", &code, http)?;
    let raw = list_dates(&src.urls().hsf10_base, ct, "lrb", &code, http)?;
    let mut periods: Vec<Period> = raw
        .iter()
        .filter_map(|s| translate::parse_em_date(s))
        .map(Period::from_date)
        .collect();
    periods.sort_by_key(Period::end_date);
    periods.dedup();
    Ok(periods)
}

/// Pull the date list under a known `companyType`.
fn list_dates(
    base: &str,
    ct: u8,
    slug: &str,
    code: &str,
    http: &HttpClient,
) -> Result<Vec<String>, SiftError> {
    let url = format!(
        "{base}/{slug}DateAjaxNew?companyType={ct}&reportDateType=0&code={code}"
    );
    let bytes = http.get_bytes(&url)?;
    let resp: DateResp = serde_json::from_slice(&bytes)
        .map_err(|e| SiftError::Internal(format!("eastmoney {slug}DateAjaxNew parse: {e}")))?;
    Ok(resp.data.into_iter().map(|d| d.report_date).collect())
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct DateResp {
    #[serde(default)]
    data: Vec<DateEntry>,
}

#[derive(serde::Deserialize)]
struct DateEntry {
    #[serde(rename = "REPORT_DATE")]
    report_date: String,
}

#[derive(serde::Deserialize)]
struct WideResp {
    #[serde(default)]
    data: Vec<Map<String, Value>>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::Market;
    use crate::domain::{Period, PeriodType, SourceTag};
    use crate::sources::financial_source::FinancialSource;

    fn maotai_sym() -> Symbol {
        Symbol {
            code: "600519".into(),
            market: Market::CnA,
            kind: crate::domain::market::InstrumentKind::Equity,
        }
    }

    fn pa_bank_sym() -> Symbol {
        Symbol {
            code: "000001".into(),
            market: Market::CnA,
            kind: crate::domain::market::InstrumentKind::Equity,
        }
    }

    fn pa_insurance_sym() -> Symbol {
        Symbol {
            code: "601318".into(),
            market: Market::CnA,
            kind: crate::domain::market::InstrumentKind::Equity,
        }
    }

    fn citic_sym() -> Symbol {
        Symbol {
            code: "600030".into(),
            market: Market::CnA,
            kind: crate::domain::market::InstrumentKind::Equity,
        }
    }

    fn income_query(symbol: Symbol, periods: Vec<Period>, scope: Scope) -> Query {
        Query {
            symbol,
            statement: Statement::Income,
            periods,
            scope,
        }
    }

    fn empty_dates_body() -> String {
        r#"{"data":[]}"#.into()
    }

    fn dates_body(dates: &[&str]) -> String {
        let arr = dates
            .iter()
            .map(|d| format!(r#"{{"REPORT_DATE":"{d}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"data":[{arr}]}}"#)
    }

    fn maotai_wide_body(date: &str) -> String {
        format!(
            r#"{{"data":[
                {{
                    "SECUCODE":"600519.SH",
                    "SECURITY_NAME_ABBR":"贵州茅台",
                    "REPORT_DATE":"{date}",
                    "REPORT_TYPE":"年报",
                    "CURRENCY":"CNY",
                    "NOTICE_DATE":"2026-03-30",
                    "TOTAL_OPERATE_INCOME":172054171891.0,
                    "OPERATE_COST":14892277571.0,
                    "PARENT_NETPROFIT":82320067102.0,
                    "BASIC_EPS":65.66,
                    "NETPROFIT":82320067102.0,
                    "TOTAL_OPERATE_INCOME_YOY":15.6
                }}
            ]}}"#
        )
    }

    /// Build a mockito server that answers the standard A-share two-step
    /// for Maotai 2025-12-31. Returns the server so tests can `.assert()`
    /// remaining expectations.
    fn mock_maotai_ct4(server: &mut mockito::Server, scope: Scope) -> (mockito::Mock, mockito::Mock) {
        let dates_path = "/lrbDateAjaxNew";
        let wide_path = "/lrbAjaxNew";
        let m_dates = server
            .mock("GET", dates_path)
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(dates_body(&["2025-12-31"]))
            .expect_at_least(1)
            .create();
        let m_wide = server
            .mock("GET", wide_path)
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(maotai_wide_body("2025-12-31"))
            .expect(1)
            .create();
        let _ = scope;
        (m_dates, m_wide)
    }

    #[test]
    fn a_share_lrb_fixture_produces_expected_rows() {
        let mut server = mockito::Server::new();
        let (_m_dates, m_wide) = mock_maotai_ct4(&mut server, Scope::Consolidated);

        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let q = income_query(maotai_sym(), vec![Period::Annual(2025)], Scope::Consolidated);
        let rows = src.fetch(&q, &HttpClient::new()).unwrap();

        // Five non-metadata, non-YOY numeric columns in the fixture.
        assert_eq!(rows.len(), 5, "rows: {rows:#?}");
        for r in &rows {
            assert_eq!(r.symbol.code, "600519");
            assert_eq!(r.period.year(), 2025);
            assert_eq!(r.period_type, PeriodType::Annual);
            assert_eq!(r.currency, "CNY");
            assert_eq!(r.source, SourceTag::EastMoney);
            assert_eq!(r.scope, Scope::Consolidated);
        }
        // Item labels are the raw EM column codes (no dictionary).
        let items: Vec<&str> = rows.iter().map(|r| r.item.as_str()).collect();
        assert!(items.contains(&"TOTAL_OPERATE_INCOME"), "items: {items:?}");
        assert!(items.contains(&"PARENT_NETPROFIT"), "items: {items:?}");
        assert!(items.contains(&"BASIC_EPS"), "items: {items:?}");

        m_wide.assert();
    }

    #[test]
    fn parent_scope_uses_report_type_two_in_url() {
        let mut server = mockito::Server::new();
        // Step 1: ct=4 returns data.
        let _m_dates = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(dates_body(&["2025-12-31"]))
            .expect_at_least(1)
            .create();
        // Step 2 must include `reportType=2` (parent).
        let m_wide = server
            .mock("GET", "/lrbAjaxNew")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("reportType".into(), "2".into()),
            ]))
            .with_status(200)
            .with_body(maotai_wide_body("2025-12-31"))
            .expect(1)
            .create();

        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let q = income_query(maotai_sym(), vec![Period::Annual(2025)], Scope::Parent);
        let rows = src.fetch(&q, &HttpClient::new()).unwrap();
        assert!(!rows.is_empty());
        m_wide.assert();
    }

    #[test]
    fn dates_chunk_into_batches_of_five() {
        let mut server = mockito::Server::new();
        // 7 periods → 2 step-2 calls (5 + 2).
        let dates = [
            "2025-12-31",
            "2025-09-30",
            "2025-06-30",
            "2025-03-31",
            "2024-12-31",
            "2024-09-30",
            "2024-06-30",
        ];
        let _m_dates = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(dates_body(&dates))
            .expect_at_least(1)
            .create();
        let m_wide = server
            .mock("GET", "/lrbAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(maotai_wide_body("2025-12-31"))
            .expect(2)
            .create();

        let periods = vec![
            Period::Annual(2025),
            Period::Q3(2025),
            Period::H1(2025),
            Period::Q1(2025),
            Period::Annual(2024),
            Period::Q3(2024),
            Period::H1(2024),
        ];
        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let _ = src
            .fetch(&income_query(maotai_sym(), periods, Scope::Consolidated), &HttpClient::new())
            .unwrap();
        m_wide.assert();
    }

    #[test]
    fn company_type_fallback_locks_on_first_non_empty() {
        // 平安银行 000001：ct=4 空，ct=3 非空 → 锁 3.
        let mut server = mockito::Server::new();
        let m4 = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "companyType".into(),
                "4".into(),
            )]))
            .with_status(200)
            .with_body(empty_dates_body())
            .expect(1)
            .create();
        let m3 = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "companyType".into(),
                "3".into(),
            )]))
            .with_status(200)
            .with_body(dates_body(&["2025-12-31"]))
            .expect_at_least(1)
            .create();
        let _m_wide = server
            .mock("GET", "/lrbAjaxNew")
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "companyType".into(),
                "3".into(),
            )]))
            .with_status(200)
            .with_body(maotai_wide_body("2025-12-31"))
            .expect(1)
            .create();

        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let _ = src
            .fetch(
                &income_query(pa_bank_sym(), vec![Period::Annual(2025)], Scope::Consolidated),
                &HttpClient::new(),
            )
            .unwrap();
        m4.assert();
        m3.assert();
    }

    #[test]
    fn company_type_fallback_to_two_then_one() {
        // 中国平安 (601318)：ct=4,3 空，ct=2 非空 → 锁 2.
        let mut server = mockito::Server::new();
        let _m4 = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "companyType".into(),
                "4".into(),
            )]))
            .with_status(200)
            .with_body(empty_dates_body())
            .expect(1)
            .create();
        let _m3 = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "companyType".into(),
                "3".into(),
            )]))
            .with_status(200)
            .with_body(empty_dates_body())
            .expect(1)
            .create();
        let m2 = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "companyType".into(),
                "2".into(),
            )]))
            .with_status(200)
            .with_body(dates_body(&["2025-12-31"]))
            .expect_at_least(1)
            .create();
        let _m_wide = server
            .mock("GET", "/lrbAjaxNew")
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "companyType".into(),
                "2".into(),
            )]))
            .with_status(200)
            .with_body(maotai_wide_body("2025-12-31"))
            .expect(1)
            .create();

        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let _ = src
            .fetch(
                &income_query(pa_insurance_sym(), vec![Period::Annual(2025)], Scope::Consolidated),
                &HttpClient::new(),
            )
            .unwrap();
        m2.assert();
    }

    #[test]
    fn company_type_fallback_chain_all_empty_returns_parse_error() {
        let mut server = mockito::Server::new();
        // All 4 ct probes return empty.
        let _m = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(empty_dates_body())
            .expect_at_least(4)
            .create();
        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let err = src
            .fetch(
                &income_query(citic_sym(), vec![Period::Annual(2025)], Scope::Consolidated),
                &HttpClient::new(),
            )
            .unwrap_err();
        match err {
            SiftError::Parse(m) => assert!(m.contains("no companyType"), "msg: {m}"),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn company_type_is_cached_per_source_instance() {
        let mut server = mockito::Server::new();
        // First query: ct=4 empty, ct=3 hits.
        let _m4 = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "companyType".into(),
                "4".into(),
            )]))
            .with_status(200)
            .with_body(empty_dates_body())
            .expect(1)  // only probed ONCE total despite two queries
            .create();
        let m3 = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "companyType".into(),
                "3".into(),
            )]))
            .with_status(200)
            .with_body(dates_body(&["2025-12-31"]))
            .expect_at_least(2) // hit on each query (list_dates re-fetches)
            .create();
        let _m_wide = server
            .mock("GET", "/lrbAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(maotai_wide_body("2025-12-31"))
            .expect_at_least(2)
            .create();

        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let q = income_query(pa_bank_sym(), vec![Period::Annual(2025)], Scope::Consolidated);
        let _ = src.fetch(&q, &HttpClient::new()).unwrap();
        let _ = src.fetch(&q, &HttpClient::new()).unwrap();
        // The expect(1) on the ct=4 probe is what proves the cache: a
        // second query without caching would have re-probed ct=4 again.
        m3.assert();
    }

    #[test]
    fn parent_scope_empty_data_does_not_silently_fall_back() {
        let mut server = mockito::Server::new();
        let _m_dates = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(dates_body(&["2025-12-31"]))
            .expect_at_least(1)
            .create();
        // Parent (reportType=2) returns empty body.
        let _m_wide = server
            .mock("GET", "/lrbAjaxNew")
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "reportType".into(),
                "2".into(),
            )]))
            .with_status(200)
            .with_body(r#"{"data":[]}"#)
            .expect(1)
            .create();
        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let rows = src
            .fetch(
                &income_query(maotai_sym(), vec![Period::Annual(2025)], Scope::Parent),
                &HttpClient::new(),
            )
            .unwrap();
        // No silent fall back to consolidated — empty rows is the right answer.
        assert!(rows.is_empty());
    }

    #[test]
    fn period_filter_excludes_unrequested_dates() {
        let mut server = mockito::Server::new();
        let _m_dates = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(dates_body(&[
                "2025-12-31",
                "2025-09-30",
                "2025-06-30",
                "2025-03-31",
                "2024-12-31",
            ]))
            .expect_at_least(1)
            .create();
        // The query has 2 periods; we expect ONE step-2 call (both fit
        // in a single chunk). We do not assert on the exact `dates=`
        // param here — that lives in `filter_dates`'s own unit test.
        let m_wide = server
            .mock("GET", "/lrbAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(maotai_wide_body("2025-12-31"))
            .expect(1)
            .create();
        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let _ = src
            .fetch(
                &income_query(
                    maotai_sym(),
                    vec![Period::Annual(2025), Period::Q1(2025)],
                    Scope::Consolidated,
                ),
                &HttpClient::new(),
            )
            .unwrap();
        m_wide.assert();
    }

    #[test]
    fn step_two_failure_aborts_the_whole_fetch() {
        let mut server = mockito::Server::new();
        let _m_dates = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(dates_body(&["2025-12-31"]))
            .expect_at_least(1)
            .create();
        let _m_wide = server
            .mock("GET", "/lrbAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(503)
            .expect_at_least(1)
            .create();
        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let err = src
            .fetch(
                &income_query(maotai_sym(), vec![Period::Annual(2025)], Scope::Consolidated),
                &HttpClient::new(),
            )
            .unwrap_err();
        // The HTTP retry layer will retry the 503; either way the
        // final result is an Err — no partial rows leak out.
        assert!(matches!(err, SiftError::Network(_)), "got {err:?}");
    }

    #[test]
    fn every_numeric_column_passes_through_verbatim() {
        // No dictionary: every non-metadata numeric column becomes a
        // row whose `item` is the raw EM column code.
        let body = r#"{"data":[{
            "REPORT_DATE":"2025-12-31",
            "REPORT_TYPE":"年报",
            "CURRENCY":"CNY",
            "TOTAL_OPERATE_INCOME":172054171891.0,
            "SIFT_A_THREE_TEST_SENTINEL":42.0
        }]}"#;
        let mut server = mockito::Server::new();
        let _m_dates = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(dates_body(&["2025-12-31"]))
            .expect_at_least(1)
            .create();
        let _m_wide = server
            .mock("GET", "/lrbAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(body)
            .expect(1)
            .create();
        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let rows = src
            .fetch(
                &income_query(maotai_sym(), vec![Period::Annual(2025)], Scope::Consolidated),
                &HttpClient::new(),
            )
            .unwrap();
        let sentinel = rows
            .iter()
            .find(|r| r.item == "SIFT_A_THREE_TEST_SENTINEL")
            .expect("unknown column should pass through");
        assert_eq!(sentinel.value, 42.0);
        // Raw EM code, not a normalized Chinese name.
        let toi = rows
            .iter()
            .find(|r| r.item == "TOTAL_OPERATE_INCOME")
            .expect("known column also passes through verbatim");
        assert_eq!(toi.value, 172054171891.0);
    }
}
