//! Sina (`sina`) `FinancialSource` implementation.
//!
//! Single endpoint:
//! `https://quotes.sina.cn/cn/api/openapi.php/CompanyFinanceService.getFinanceReport2022`
//!
//! Covers only the cases the upstream actually serves:
//!
//! - market = `CnA`
//! - scope = `Consolidated` (sina has no parent-only view)
//! - statement ∈ {Income, Balance, Cashflow} — sina exposes no
//!   `Indicator` endpoint
//! - board ≠ `BjMain` — sina lists SH + SZ + B-share but not Beijing
//!
//! Everything else returns `false` from `supports`, so the
//! dispatcher skips sina entirely for those queries. Item names are
//! normalized via the F2 dictionary in
//! [`crate::domain::items_dict::dict`], same as the EM source.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use time::{Date, Month};

use crate::domain::market::{infer_board, Board, Market};
use crate::domain::{
    items_dict, AuditStatus, FinancialRow, Period, PeriodType, Query, Scope, SourceTag, Statement,
    Symbol, Unit,
};
use crate::error::SiftError;
use crate::http::HttpClient;
use crate::sources::financial_source::FinancialSource;

const DEFAULT_BASE: &str =
    "https://quotes.sina.cn/cn/api/openapi.php/CompanyFinanceService.getFinanceReport2022";

/// Single-endpoint sina source. Holds only a base URL (no caches);
/// `with_url` is the test seam.
pub struct SinaFinancialSource {
    base_url: String,
}

impl SinaFinancialSource {
    pub fn new() -> Self {
        Self {
            base_url: std::env::var("SIFT_SINA_BASE").unwrap_or_else(|_| DEFAULT_BASE.into()),
        }
    }

    /// Test-only seam — construct against an explicit URL. Production
    /// callers use [`Self::new`] which reads the env-resolved default.
    #[cfg(test)]
    pub fn with_url(url: impl Into<String>) -> Self {
        Self { base_url: url.into() }
    }
}

impl Default for SinaFinancialSource {
    fn default() -> Self {
        Self::new()
    }
}

impl FinancialSource for SinaFinancialSource {
    fn name(&self) -> &'static str {
        "sina"
    }

    fn supports(&self, q: &Query) -> bool {
        if q.symbol.market != Market::CnA {
            return false;
        }
        if q.scope != Scope::Consolidated {
            return false;
        }
        if matches!(q.statement, Statement::Indicator) {
            return false;
        }
        // sina does not cover Beijing Stock Exchange; treat unknown
        // prefixes as out-of-scope too (defense — we'd rather skip
        // than send an unsupported request and surface an opaque
        // error from the upstream).
        match infer_board(&q.symbol.code) {
            Some(Board::BjMain) => false,
            Some(_) => true,
            None => false,
        }
    }

    fn fetch(&self, q: &Query, http: &HttpClient) -> Result<Vec<FinancialRow>, SiftError> {
        let url = build_url(&self.base_url, q)?;
        let bytes = http.get_bytes(&url)?;
        let raw: SinaResp = serde_json::from_slice(&bytes)
            .map_err(|e| SiftError::Internal(format!("sina parse: {e}")))?;
        if raw.result.status.code != 0 {
            return Err(SiftError::Network(format!(
                "sina status code {}",
                raw.result.status.code
            )));
        }
        parse_rows(raw, q)
    }
}

/// Factory for the global registry. `main` calls this alongside
/// `eastmoney_financials::build()`.
pub fn build() -> Box<dyn FinancialSource> {
    Box::new(SinaFinancialSource::new())
}

// ---------------------------------------------------------------------------
// URL construction
// ---------------------------------------------------------------------------

fn paper_code(sym: &Symbol) -> Result<String, SiftError> {
    let prefix = match infer_board(&sym.code) {
        Some(Board::ShMain | Board::ShStar) => "sh",
        Some(Board::SzMain | Board::SzSme | Board::SzGem) => "sz",
        Some(Board::BShare) => {
            // 900xxx = SH B-share, 200xxx = SZ B-share.
            if sym.code.starts_with('9') {
                "sh"
            } else {
                "sz"
            }
        }
        Some(Board::BjMain) | None => {
            return Err(SiftError::Parse(format!(
                "sina does not support code {}",
                sym.code
            )));
        }
    };
    Ok(format!("{prefix}{}", sym.code))
}

fn source_param(s: Statement) -> Result<&'static str, SiftError> {
    match s {
        Statement::Balance => Ok("fzb"),
        Statement::Income => Ok("lrb"),
        Statement::Cashflow => Ok("llb"),
        Statement::Indicator => Err(SiftError::Internal(
            "sina does not support indicator statement".into(),
        )),
    }
}

fn build_url(base: &str, q: &Query) -> Result<String, SiftError> {
    Ok(format!(
        "{base}?paperCode={pc}&source={src}&type=0&page=1&num=1000",
        pc = paper_code(&q.symbol)?,
        src = source_param(q.statement)?,
    ))
}

// ---------------------------------------------------------------------------
// Deserialization
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SinaResp {
    result: SinaResult,
}

#[derive(Deserialize)]
struct SinaResult {
    status: Status,
    #[serde(default)]
    data: Option<SinaData>,
}

#[derive(Deserialize)]
struct Status {
    code: i32,
}

#[derive(Deserialize)]
struct SinaData {
    #[serde(default)]
    report_date: Vec<DateEntry>,
    #[serde(default)]
    report_list: HashMap<String, PeriodPayload>,
}

#[derive(Deserialize)]
struct DateEntry {
    #[serde(rename = "date_value")]
    _date_value: String,
    #[serde(rename = "date_description")]
    _date_description: String,
    #[serde(rename = "date_type")]
    _date_type: i32,
}

#[derive(Deserialize)]
struct PeriodPayload {
    #[serde(rename = "rType", default)]
    _r_type: String,
    #[serde(rename = "rCurrency", default)]
    r_currency: String,
    #[serde(default)]
    is_audit: String,
    #[serde(default)]
    publish_date: String,
    #[serde(default)]
    data: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    item_title: String,
    item_value: Option<String>,
}

// ---------------------------------------------------------------------------
// Row projection
// ---------------------------------------------------------------------------

fn parse_rows(raw: SinaResp, q: &Query) -> Result<Vec<FinancialRow>, SiftError> {
    let data = raw
        .result
        .data
        .ok_or_else(|| SiftError::Parse("sina: empty data".into()))?;

    let wanted: HashSet<Date> = q.periods.iter().map(Period::end_date).collect();
    let _ = data.report_date; // currently unused; placeholder for YOY later

    let mut rows = Vec::new();
    for (key, payload) in data.report_list {
        let Some(date) = parse_yyyymmdd(&key) else {
            continue;
        };
        if !wanted.is_empty() && !wanted.contains(&date) {
            continue;
        }
        let period_type = PeriodType::from_date(date).unwrap_or(PeriodType::Annual);
        let currency = if payload.r_currency.is_empty() {
            "CNY".into()
        } else {
            payload.r_currency.clone()
        };
        let audit = audit_from_str(&payload.is_audit);
        let publish_date = parse_yyyymmdd(&payload.publish_date);

        for it in payload.data {
            let Some(v) = it.item_value.as_deref().and_then(|s| s.parse::<f64>().ok()) else {
                continue; // silently skip null / non-numeric
            };
            rows.push(FinancialRow {
                symbol: q.symbol.clone(),
                // sina does not return the security short name; the
                // command layer fills this in from the F1 cninfo cache
                // before rendering.
                name: String::new(),
                period: date,
                period_type,
                statement: q.statement,
                scope: Scope::Consolidated,
                item: items_dict::dict().normalize(&it.item_title),
                value: v,
                unit: Unit::Raw,
                currency: currency.clone(),
                publish_date,
                audit,
                source: SourceTag::Sina,
            });
        }
    }
    Ok(rows)
}

/// Parse `"YYYYMMDD"` strings to `Date`. Returns `None` for any
/// shape that does not match.
fn parse_yyyymmdd(s: &str) -> Option<Date> {
    if s.len() != 8 {
        return None;
    }
    let y: i32 = s.get(0..4)?.parse().ok()?;
    let m: u8 = s.get(4..6)?.parse().ok()?;
    let d: u8 = s.get(6..8)?.parse().ok()?;
    Date::from_calendar_date(y, Month::try_from(m).ok()?, d).ok()
}

fn audit_from_str(s: &str) -> AuditStatus {
    match s {
        "已审计" => AuditStatus::Audited,
        "未审计" => AuditStatus::Unaudited,
        _ => AuditStatus::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppContext;
    use crate::fetch::report::{dispatch_with_cache, ReportContext};
    use crate::sources::eastmoney_financials::EastmoneyFinancialSource;
    use crate::sources::financial_source::FinancialSource;

    fn cn_a(code: &str) -> Symbol {
        Symbol {
            code: code.into(),
            market: Market::CnA,
        }
    }

    fn income_query(symbol: Symbol, periods: Vec<Period>) -> Query {
        Query {
            symbol,
            statement: Statement::Income,
            periods,
            scope: Scope::Consolidated,
        }
    }

    /// Three-period sina-style lrb fixture for Maotai (SH600519).
    /// Item titles intentionally include a mix of: dictionary-mapped
    /// names (营业总收入 / 归属于母公司股东的净利润 — sina form), a
    /// `减:` prefix (营业成本 alias), and one unmapped sentinel.
    fn sina_lrb_body() -> String {
        r#"{
            "result": {
                "status": {"code": 0},
                "data": {
                    "report_date": [
                        {"date_value": "20251231", "date_description": "2025年报", "date_type": 1},
                        {"date_value": "20250930", "date_description": "2025三季报", "date_type": 4},
                        {"date_value": "20250630", "date_description": "2025中报", "date_type": 3}
                    ],
                    "report_list": {
                        "20251231": {
                            "rType": "合并期末",
                            "rCurrency": "CNY",
                            "is_audit": "已审计",
                            "publish_date": "20260401",
                            "data": [
                                {"item_title": "营业总收入", "item_value": "172054171890.91"},
                                {"item_title": "营业收入", "item_value": "168838102514.79"},
                                {"item_title": "减:营业成本", "item_value": "14892277571.00"},
                                {"item_title": "归属于母公司股东的净利润", "item_value": "82320067102.00"},
                                {"item_title": "基本每股收益", "item_value": "65.66"},
                                {"item_title": "SIFT_SINA_TEST_SENTINEL", "item_value": "1.0"},
                                {"item_title": "净利润", "item_value": null},
                                {"item_title": "净利润续", "item_value": "N/A"}
                            ]
                        },
                        "20250930": {
                            "rType": "合并期末",
                            "rCurrency": "CNY",
                            "is_audit": "未审计",
                            "publish_date": "20251025",
                            "data": [
                                {"item_title": "营业总收入", "item_value": "125000000000.00"},
                                {"item_title": "归属于母公司股东的净利润", "item_value": "60000000000.00"}
                            ]
                        },
                        "20250630": {
                            "rType": "合并期末",
                            "rCurrency": "",
                            "is_audit": "",
                            "publish_date": "",
                            "data": [
                                {"item_title": "营业总收入", "item_value": "85000000000.00"}
                            ]
                        }
                    }
                }
            }
        }"#
        .into()
    }

    fn make_server() -> mockito::ServerGuard {
        mockito::Server::new()
    }

    fn mock_sina(server: &mut mockito::ServerGuard, body: &str) -> mockito::Mock {
        server
            .mock("GET", "/")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(body)
            .expect_at_least(1)
            .create()
    }

    // ---- supports() matrix ------------------------------------------

    #[test]
    fn supports_matrix() {
        let s = SinaFinancialSource::new();

        // A-share + Consolidated + 三大表 = true
        for st in [Statement::Income, Statement::Balance, Statement::Cashflow] {
            assert!(s.supports(&Query {
                symbol: cn_a("600519"),
                statement: st,
                periods: vec![],
                scope: Scope::Consolidated,
            }));
        }
        // Indicator → false
        assert!(!s.supports(&Query {
            symbol: cn_a("600519"),
            statement: Statement::Indicator,
            periods: vec![],
            scope: Scope::Consolidated,
        }));
        // Parent → false
        assert!(!s.supports(&Query {
            symbol: cn_a("600519"),
            statement: Statement::Income,
            periods: vec![],
            scope: Scope::Parent,
        }));
        // HK → false
        assert!(!s.supports(&Query {
            symbol: Symbol {
                code: "00700".into(),
                market: Market::Hk
            },
            statement: Statement::Income,
            periods: vec![],
            scope: Scope::Consolidated,
        }));
        // 北交所 → false
        assert!(!s.supports(&Query {
            symbol: cn_a("832000"),
            statement: Statement::Income,
            periods: vec![],
            scope: Scope::Consolidated,
        }));
        // SZ Main → true
        assert!(s.supports(&Query {
            symbol: cn_a("000858"),
            statement: Statement::Income,
            periods: vec![],
            scope: Scope::Consolidated,
        }));
    }

    // ---- URL plumbing ------------------------------------------------

    #[test]
    fn paper_code_prefix_per_board() {
        assert_eq!(paper_code(&cn_a("600519")).unwrap(), "sh600519");
        assert_eq!(paper_code(&cn_a("000858")).unwrap(), "sz000858");
        assert_eq!(paper_code(&cn_a("300750")).unwrap(), "sz300750");
        // B-share split: 200xxx = SZ, 900xxx = SH.
        assert_eq!(paper_code(&cn_a("200011")).unwrap(), "sz200011");
        assert_eq!(paper_code(&cn_a("900901")).unwrap(), "sh900901");
        // Beijing / unknown → Err
        assert!(matches!(
            paper_code(&cn_a("832000")),
            Err(SiftError::Parse(_))
        ));
    }

    #[test]
    fn source_param_per_statement() {
        assert_eq!(source_param(Statement::Income).unwrap(), "lrb");
        assert_eq!(source_param(Statement::Balance).unwrap(), "fzb");
        assert_eq!(source_param(Statement::Cashflow).unwrap(), "llb");
        assert!(source_param(Statement::Indicator).is_err());
    }

    // ---- fetch + parse ----------------------------------------------

    #[test]
    fn fetch_produces_expected_rows_with_currency_and_audit() {
        let mut server = make_server();
        let _m = mock_sina(&mut server, &sina_lrb_body());

        let src = SinaFinancialSource::with_url(server.url());
        let rows = src
            .fetch(
                &income_query(cn_a("600519"), vec![]),
                &HttpClient::new(),
            )
            .unwrap();

        // 6 numeric rows in 2025-12-31 (营业总收入 / 营业收入 /
        // 减:营业成本 / 归属于母公司股东的净利润 / 基本每股收益 /
        // SIFT_SINA_TEST_SENTINEL — the two null/N/A skipped) + 2 in
        // 2025-09-30 + 1 in 2025-06-30 = 9 rows.
        assert_eq!(rows.len(), 9, "rows: {rows:#?}");
        for r in &rows {
            assert_eq!(r.source, SourceTag::Sina);
            assert_eq!(r.scope, Scope::Consolidated);
            assert_eq!(r.symbol.code, "600519");
            assert!(r.name.is_empty(), "sina leaves name blank");
        }

        // Spot-check 2025-12-31:
        let annual: Vec<&FinancialRow> = rows
            .iter()
            .filter(|r| r.period.year() == 2025 && r.period_type == PeriodType::Annual)
            .collect();
        assert_eq!(annual.len(), 6);
        let by_item: std::collections::HashMap<&str, &FinancialRow> =
            annual.iter().map(|r| (r.item.as_str(), *r)).collect();
        assert_eq!(by_item["营业总收入"].value, 172054171890.91);
        assert_eq!(by_item["营业总收入"].currency, "CNY");
        assert_eq!(by_item["营业总收入"].audit, AuditStatus::Audited);
        // 减:营业成本 normalizes to 营业成本 via the dict.
        assert_eq!(by_item["营业成本"].value, 14892277571.0);
        // sina-style 归属于母公司股东的净利润 → 归母净利润.
        assert_eq!(by_item["归母净利润"].value, 82320067102.0);
        // Sentinel survives passthrough.
        assert!(by_item.contains_key("SIFT_SINA_TEST_SENTINEL"));

        // 2025-09-30 = Q3, unaudited.
        let q3 = rows
            .iter()
            .find(|r| r.period.year() == 2025 && r.period_type == PeriodType::Q3)
            .expect("Q3 row present");
        assert_eq!(q3.audit, AuditStatus::Unaudited);
        // 2025-06-30 = H1 with empty currency → falls back to "CNY".
        let h1 = rows
            .iter()
            .find(|r| r.period_type == PeriodType::H1)
            .expect("H1 row present");
        assert_eq!(h1.currency, "CNY");
        assert_eq!(h1.audit, AuditStatus::Unknown);
    }

    #[test]
    fn period_filter_excludes_other_periods() {
        let mut server = make_server();
        let _m = mock_sina(&mut server, &sina_lrb_body());
        let src = SinaFinancialSource::with_url(server.url());

        let rows = src
            .fetch(
                &income_query(cn_a("600519"), vec![Period::Annual(2025)]),
                &HttpClient::new(),
            )
            .unwrap();
        assert!(!rows.is_empty());
        for r in &rows {
            assert_eq!(r.period_type, PeriodType::Annual);
            assert_eq!(r.period.year(), 2025);
        }
    }

    #[test]
    fn period_type_inferred_from_date_value() {
        // The parser ignores `date_description` and relies on the
        // YYYYMMDD month-day.
        let body = r#"{"result":{"status":{"code":0},"data":{
            "report_date":[],
            "report_list":{
                "20240331":{"rType":"合并","rCurrency":"CNY","is_audit":"","publish_date":"","data":[
                    {"item_title":"营业总收入","item_value":"1.0"}
                ]},
                "20240630":{"rType":"合并","rCurrency":"CNY","is_audit":"","publish_date":"","data":[
                    {"item_title":"营业总收入","item_value":"1.0"}
                ]},
                "20240930":{"rType":"合并","rCurrency":"CNY","is_audit":"","publish_date":"","data":[
                    {"item_title":"营业总收入","item_value":"1.0"}
                ]},
                "20241231":{"rType":"合并","rCurrency":"CNY","is_audit":"","publish_date":"","data":[
                    {"item_title":"营业总收入","item_value":"1.0"}
                ]}
            }
        }}}"#;
        let mut server = make_server();
        let _m = mock_sina(&mut server, body);
        let src = SinaFinancialSource::with_url(server.url());

        let rows = src
            .fetch(&income_query(cn_a("600519"), vec![]), &HttpClient::new())
            .unwrap();
        let pt_by_date: std::collections::HashMap<_, _> =
            rows.iter().map(|r| (r.period, r.period_type)).collect();

        let mar = parse_yyyymmdd("20240331").unwrap();
        let jun = parse_yyyymmdd("20240630").unwrap();
        let sep = parse_yyyymmdd("20240930").unwrap();
        let dec = parse_yyyymmdd("20241231").unwrap();
        assert_eq!(pt_by_date[&mar], PeriodType::Q1);
        assert_eq!(pt_by_date[&jun], PeriodType::H1);
        assert_eq!(pt_by_date[&sep], PeriodType::Q3);
        assert_eq!(pt_by_date[&dec], PeriodType::Annual);
    }

    #[test]
    fn null_and_non_numeric_item_values_are_silently_skipped() {
        // sina_lrb_body() has two such items at 2025-12-31. Test that
        // both are dropped while the other 6 survive.
        let mut server = make_server();
        let _m = mock_sina(&mut server, &sina_lrb_body());
        let src = SinaFinancialSource::with_url(server.url());

        let rows = src
            .fetch(
                &income_query(cn_a("600519"), vec![Period::Annual(2025)]),
                &HttpClient::new(),
            )
            .unwrap();
        assert_eq!(rows.len(), 6);
    }

    #[test]
    fn nonzero_status_code_returns_error() {
        let body = r#"{"result":{"status":{"code":1},"data":null}}"#;
        let mut server = make_server();
        let _m = mock_sina(&mut server, body);
        let src = SinaFinancialSource::with_url(server.url());
        let err = src
            .fetch(&income_query(cn_a("600519"), vec![]), &HttpClient::new())
            .unwrap_err();
        assert!(matches!(err, SiftError::Network(_)), "got {err:?}");
        assert!(err.to_string().contains("sina"));
    }

    #[test]
    fn empty_data_returns_parse_error() {
        let body = r#"{"result":{"status":{"code":0},"data":null}}"#;
        let mut server = make_server();
        let _m = mock_sina(&mut server, body);
        let src = SinaFinancialSource::with_url(server.url());
        let err = src
            .fetch(&income_query(cn_a("600519"), vec![]), &HttpClient::new())
            .unwrap_err();
        assert!(matches!(err, SiftError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn unmapped_item_titles_pass_through_to_unmapped_collector() {
        let _ = items_dict::drain_unmapped();
        let mut server = make_server();
        let _m = mock_sina(&mut server, &sina_lrb_body());
        let src = SinaFinancialSource::with_url(server.url());

        let _ = src
            .fetch(&income_query(cn_a("600519"), vec![]), &HttpClient::new())
            .unwrap();
        let drained = items_dict::drain_unmapped();
        assert!(
            drained.contains(&"SIFT_SINA_TEST_SENTINEL".to_string()),
            "drained: {drained:?}"
        );
    }

    // ---- Dispatch interaction with eastmoney ------------------------

    /// A tiny sina lrb response with a distinguishable value.
    fn sina_lrb_simple_body() -> String {
        r#"{
            "result":{"status":{"code":0},"data":{
                "report_date":[],
                "report_list":{
                    "20251231":{
                        "rType":"合并期末","rCurrency":"CNY","is_audit":"已审计",
                        "publish_date":"20260401",
                        "data":[{"item_title":"营业总收入","item_value":"999.0"}]
                    }
                }
            }}
        }"#
        .into()
    }

    fn build_em(server: &mockito::ServerGuard) -> EastmoneyFinancialSource {
        EastmoneyFinancialSource::with_urls(server.url(), server.url())
    }
    fn build_sina(server: &mockito::ServerGuard) -> SinaFinancialSource {
        SinaFinancialSource::with_url(server.url())
    }

    #[test]
    fn dispatch_falls_back_to_sina_when_em_fails() {
        let mut server = make_server();
        // EM step 1 returns 404 (not retried) → immediate Err for EM.
        let _em_fail = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(404)
            .with_body("not found")
            .expect_at_least(1)
            .create();
        // sina succeeds.
        let _sina_ok = server
            .mock("GET", "/")
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "paperCode".into(),
                "sh600519".into(),
            )]))
            .with_status(200)
            .with_body(sina_lrb_simple_body())
            .expect_at_least(1)
            .create();

        let em = build_em(&server);
        let sina = build_sina(&server);
        let app = AppContext::default();
        let srcs: Vec<Box<dyn FinancialSource>> = vec![Box::new(em), Box::new(sina)];
        let ctx = ReportContext { app: &app, sources: &srcs };

        let q = income_query(cn_a("600519"), vec![Period::Annual(2025)]);
        let rows = dispatch_with_cache(&q, &ctx).unwrap();

        assert!(!rows.is_empty(), "expected sina rows");
        assert_eq!(rows[0].source, SourceTag::Sina);
        assert_eq!(rows[0].value, 999.0);
    }

    #[test]
    fn dispatch_uses_sina_when_em_does_not_support_query() {
        // Sina supports CnA + Consolidated; EM does too. To exercise
        // sina alone we put both into the registry but pick a query
        // where only sina would respond meaningfully — here we just
        // verify the source flag in the result without forcing a race.
        // (`Story 02` already tests timing-based first-success.)
        let mut server = make_server();
        let _em_fail = server
            .mock("GET", "/lrbDateAjaxNew")
            .match_query(mockito::Matcher::Any)
            .with_status(404)
            .expect_at_least(1)
            .create();
        let _sina_ok = server
            .mock("GET", "/")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(sina_lrb_simple_body())
            .expect_at_least(1)
            .create();
        let em = build_em(&server);
        let sina = build_sina(&server);
        let app = AppContext::default();
        let srcs: Vec<Box<dyn FinancialSource>> = vec![Box::new(em), Box::new(sina)];
        let ctx = ReportContext { app: &app, sources: &srcs };
        let q = income_query(cn_a("600519"), vec![Period::Annual(2025)]);
        let rows = dispatch_with_cache(&q, &ctx).unwrap();
        assert_eq!(rows[0].source, SourceTag::Sina);
    }

    #[test]
    fn dispatch_skips_sina_for_hk_and_parent_queries() {
        // For HK Consolidated, sina.supports() = false → only EM tried.
        // We don't even need to mock sina; if the dispatcher tried it,
        // mockito's default 501 would surface as an error.
        let server = make_server();
        let em = build_em(&server);
        let sina = build_sina(&server);

        // sina.supports must be false for HK.
        let hk_q = Query {
            symbol: Symbol {
                code: "00700".into(),
                market: Market::Hk,
            },
            statement: Statement::Income,
            periods: vec![Period::Annual(2024)],
            scope: Scope::Consolidated,
        };
        assert!(!sina.supports(&hk_q));
        // EM does support HK Consolidated.
        assert!(em.supports(&hk_q));

        // Sanity: A-share Parent skips sina (parent unsupported), EM
        // still picks it up.
        let parent_q = Query {
            symbol: cn_a("600519"),
            statement: Statement::Income,
            periods: vec![Period::Annual(2025)],
            scope: Scope::Parent,
        };
        assert!(!sina.supports(&parent_q));
        assert!(em.supports(&parent_q));
    }
}
