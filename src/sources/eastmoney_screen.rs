//! EastMoney whole-market业绩报表 feed (`RPT_LICO_FN_CPD`).
//!
//! One report date → one row per A-share issuer with a curated set of
//! headline metrics (revenue / net profit / ROE / EPS / margins /
//! YoY …). Powers `sift market`. Unlike the F2 per-symbol adapters
//! this is a paginated whole-market pull, so it lives outside the
//! `FinancialSource` trait as a plain function.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use time::{Date, Month};

use crate::error::SiftError;
use crate::http::HttpClient;

/// Default datacenter endpoint; overridable via `SIFT_EM_SCREEN_BASE`
/// for tests.
const DEFAULT_BASE: &str = "https://datacenter-web.eastmoney.com/api/data/v1/get";
const PAGE_SIZE: usize = 500;
/// Safety cap on pagination (≈ 24 pages cover the whole A-share market
/// at pageSize 500; the cap guards against a runaway `pages` value).
const MAX_PAGES: usize = 80;

/// Amount-typed columns (year-to-date cumulative). Mapped to
/// `qmode = cumulative` on ingest.
pub const AMOUNT_COLS: [&str; 2] = ["TOTAL_OPERATE_INCOME", "PARENT_NETPROFIT"];

/// Ratio / per-share / growth columns. Mapped to `qmode = na`.
pub const NA_COLS: [&str; 11] = [
    "WEIGHTAVG_ROE",   // 加权净资产收益率 %
    "BASIC_EPS",       // 基本每股收益
    "DEDUCT_BASIC_EPS",// 扣非每股收益
    "BPS",             // 每股净资产
    "XSMLL",           // 销售毛利率 %
    "YSTZ",            // 营业总收入同比 %
    "SJLTZ",           // 净利润同比 %
    "MGJYXJJE",        // 每股经营现金流
    "ZXGXL",           // 股息率 %
    "YSHZ",            // 营收环比 %
    "SJLHZ",           // 净利润环比 %
];

/// One issuer's snapshot for a report date. `metrics` keys are the raw
/// EM column names (both [`AMOUNT_COLS`] and [`NA_COLS`]); absent /
/// null upstream values are simply not inserted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketRow {
    pub code: String,
    pub name: String,
    pub board_name: Option<String>,
    /// Disclosure date as ISO `YYYY-MM-DD` (kept as a string so the row
    /// is serde-clean for the snapshot cache; the service parses it).
    pub notice_date: Option<String>,
    pub metrics: HashMap<String, f64>,
}

#[derive(Deserialize)]
struct Envelope {
    result: Option<ResultBlock>,
}

#[derive(Deserialize)]
struct ResultBlock {
    pages: i64,
    data: Vec<serde_json::Map<String, serde_json::Value>>,
}

/// Resolve the datacenter base (env override for tests).
pub fn base() -> String {
    std::env::var("SIFT_EM_SCREEN_BASE").unwrap_or_else(|_| DEFAULT_BASE.to_string())
}

/// Fetch the whole-market snapshot for `report_end`, following
/// pagination to completion (capped at [`MAX_PAGES`]).
pub fn fetch_snapshot(
    http: &HttpClient,
    base: &str,
    report_end: Date,
) -> Result<Vec<MarketRow>, SiftError> {
    let date = fmt_date(report_end);
    let mut out = Vec::new();
    let mut page = 1usize;
    let mut total_pages = 1usize;
    while page <= total_pages && page <= MAX_PAGES {
        let block = fetch_page(http, base, &date, page)?;
        if page == 1 {
            total_pages = block.pages.max(0) as usize;
        }
        for row in &block.data {
            if let Some(r) = parse_row(row) {
                out.push(r);
            }
        }
        if block.data.is_empty() {
            break;
        }
        page += 1;
    }
    Ok(out)
}

fn fetch_page(
    http: &HttpClient,
    base: &str,
    date: &str,
    page: usize,
) -> Result<ResultBlock, SiftError> {
    // Single quotes in the filter are percent-encoded; parens are
    // accepted raw by the endpoint.
    let url = format!(
        "{base}?reportName=RPT_LICO_FN_CPD&columns=ALL&pageSize={PAGE_SIZE}&pageNumber={page}\
         &sortColumns=SECURITY_CODE&sortTypes=1&filter=(REPORTDATE=%27{date}%27)"
    );
    let bytes = http.get_bytes(&url)?;
    let env: Envelope = serde_json::from_slice(&bytes)
        .map_err(|e| SiftError::Parse(format!("RPT_LICO_FN_CPD decode: {e}")))?;
    env.result
        .ok_or_else(|| SiftError::Parse("RPT_LICO_FN_CPD: empty result".into()))
}

fn parse_row(row: &serde_json::Map<String, serde_json::Value>) -> Option<MarketRow> {
    let code = row.get("SECURITY_CODE")?.as_str()?.to_string();
    let name = row
        .get("SECURITY_NAME_ABBR")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let board_name = row
        .get("BOARD_NAME")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let notice_date = row
        .get("NOTICE_DATE")
        .and_then(|v| v.as_str())
        .and_then(parse_em_date)
        .map(fmt_date);

    let mut metrics = HashMap::new();
    for col in AMOUNT_COLS.iter().chain(NA_COLS.iter()) {
        if let Some(v) = row.get(*col).and_then(num) {
            metrics.insert((*col).to_string(), v);
        }
    }
    Some(MarketRow {
        code,
        name,
        board_name,
        notice_date,
        metrics,
    })
}

/// Coerce a JSON cell to f64. EM returns numbers as JSON numbers but
/// occasionally as strings; `null` / `"-"` yield `None`.
fn num(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// `"2024-12-31 00:00:00"` → `Date`.
fn parse_em_date(s: &str) -> Option<Date> {
    let d = s.get(..10)?;
    let mut parts = d.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    Date::from_calendar_date(y, Month::try_from(m).ok()?, day).ok()
}

fn fmt_date(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), d.month() as u8, d.day())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    #[test]
    fn parse_row_extracts_metrics_and_skips_nulls() {
        let json = serde_json::json!({
            "SECURITY_CODE": "000001",
            "SECURITY_NAME_ABBR": "平安银行",
            "BOARD_NAME": "银行",
            "NOTICE_DATE": "2025-03-15 00:00:00",
            "TOTAL_OPERATE_INCOME": 146695000000.0,
            "PARENT_NETPROFIT": 44508000000.0,
            "WEIGHTAVG_ROE": 10.08,
            "XSMLL": serde_json::Value::Null,
            "BASIC_EPS": "2.15"
        });
        let map = json.as_object().unwrap().clone();
        let r = parse_row(&map).unwrap();
        assert_eq!(r.code, "000001");
        assert_eq!(r.name, "平安银行");
        assert_eq!(r.board_name.as_deref(), Some("银行"));
        assert_eq!(r.notice_date.as_deref(), Some("2025-03-15"));
        assert_eq!(r.metrics.get("TOTAL_OPERATE_INCOME"), Some(&146695000000.0));
        assert_eq!(r.metrics.get("WEIGHTAVG_ROE"), Some(&10.08));
        // String-encoded number is coerced.
        assert_eq!(r.metrics.get("BASIC_EPS"), Some(&2.15));
        // Null metric is skipped.
        assert!(!r.metrics.contains_key("XSMLL"));
    }

    #[test]
    fn parse_em_date_takes_date_prefix() {
        assert_eq!(
            parse_em_date("2024-12-31 00:00:00"),
            Date::from_calendar_date(2024, Month::December, 31).ok()
        );
        assert_eq!(parse_em_date("garbage"), None);
    }
}
