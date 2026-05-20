//! Shared helpers across the East Money adapters: URL prefix lookup,
//! date parsing, A-share wide-table → `FinancialRow` projection.

use std::collections::HashSet;

use serde_json::{Map, Value};
use time::{Date, Month};

use crate::domain::market::{infer_board, Board, Market};
use crate::domain::{
    items_dict, AuditStatus, FinancialRow, PeriodType, Query, SourceTag, Symbol, Unit,
};

/// Build the EM A-share `code` parameter: `{SH|SZ|BJ}{code}`.
pub fn a_share_code(symbol: &Symbol) -> String {
    debug_assert_eq!(symbol.market, Market::CnA);
    let prefix = match infer_board(&symbol.code) {
        Some(Board::ShMain | Board::ShStar) => "SH",
        Some(Board::SzMain | Board::SzSme | Board::SzGem) => "SZ",
        Some(Board::BjMain) => "BJ",
        Some(Board::BShare) => {
            // 900xxx = SH B-share, 200xxx = SZ B-share.
            if symbol.code.starts_with('9') {
                "SH"
            } else {
                "SZ"
            }
        }
        // Unknown prefix — default to SH so the URL still gets built.
        None => "SH",
    };
    format!("{prefix}{}", symbol.code)
}

/// HK / US `SECUCODE` parameter: `"{code}.{HK|US}"`.
pub fn secucode(symbol: &Symbol) -> String {
    format!("{}.{}", symbol.code, market_suffix(symbol.market))
}

fn market_suffix(m: Market) -> &'static str {
    match m {
        Market::CnA => "CN", // not actually used; A-share path is different.
        Market::Hk => "HK",
        Market::Us => "US",
    }
}

/// Accepts either `YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS` (EM
/// datacenter often returns the latter). Returns the date portion.
pub fn parse_em_date(s: &str) -> Option<Date> {
    let head = s.split(' ').next().unwrap_or("");
    let mut parts = head.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    let d: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Date::from_calendar_date(y, Month::try_from(m).ok()?, d).ok()
}

/// Map EM's Chinese `REPORT_TYPE` label to our `PeriodType`. Unknown
/// labels return `None` so the caller can fall back to inferring from
/// the date.
pub fn report_type_to_period_type(label: &str) -> Option<PeriodType> {
    match label {
        "年报" | "年度" | "年度报告" => Some(PeriodType::Annual),
        "中报" | "半年报" | "中期" => Some(PeriodType::H1),
        "一季报" | "1季报" | "第一季报" => Some(PeriodType::Q1),
        "三季报" | "3季报" | "第三季报" => Some(PeriodType::Q3),
        _ => None,
    }
}

/// Read a numeric field that may arrive as a JSON number or a
/// stringified number. Anything else (Null / Bool / Array / Object)
/// returns `None`.
pub fn extract_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Metadata column names that should never become a `FinancialRow`
/// item. EM A-share wide tables also include sector-specific metadata
/// (`SECURITY_TYPE_CODE`, `ORG_TYPE`); we list every name we have
/// observed and treat unknown all-uppercase strings as data otherwise.
pub fn is_metadata_col(name: &str) -> bool {
    A_META_COLS.contains(&name)
}

const A_META_COLS: &[&str] = &[
    "SECUCODE",
    "SECURITY_CODE",
    "SECURITY_NAME_ABBR",
    "ORG_CODE",
    "ORG_TYPE",
    "REPORT_DATE",
    "REPORT_TYPE",
    "REPORT_DATE_NAME",
    "SECURITY_TYPE_CODE",
    "NOTICE_DATE",
    "UPDATE_DATE",
    "CURRENCY",
    // Variants observed across sector templates.
    "MARKET",
    "MARKET_NAME",
    "DATE_TYPE",
    "DATE_TYPE_CODE",
];

/// YOY (year-over-year) suffix; first-version output skips these
/// columns. A later story can enable them as extra `FinancialRow`s.
const A_YOY_SUFFIX: &str = "_YOY";

/// Translate one A-share wide-table row (one `REPORT_DATE`) into many
/// `FinancialRow`s. Returns `Err` only if the row is missing
/// `REPORT_DATE` — every other field falls back to a sane default.
pub fn a_wide_to_rows(entry: &Map<String, Value>, q: &Query) -> Vec<FinancialRow> {
    let Some(date) = entry
        .get("REPORT_DATE")
        .and_then(|v| v.as_str())
        .and_then(parse_em_date)
    else {
        return Vec::new();
    };
    let report_type_str = entry
        .get("REPORT_TYPE")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let period_type = report_type_to_period_type(report_type_str)
        .or_else(|| PeriodType::from_date(date))
        .unwrap_or(PeriodType::Annual);
    let currency = entry
        .get("CURRENCY")
        .and_then(|v| v.as_str())
        .unwrap_or("CNY")
        .to_string();
    let publish_date = entry
        .get("NOTICE_DATE")
        .and_then(|v| v.as_str())
        .and_then(parse_em_date);
    let name = entry
        .get("SECURITY_NAME_ABBR")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut rows = Vec::new();
    for (key, value) in entry {
        if is_metadata_col(key) || key.ends_with(A_YOY_SUFFIX) {
            continue;
        }
        let Some(num) = extract_number(value) else {
            continue;
        };
        rows.push(FinancialRow {
            symbol: q.symbol.clone(),
            name: name.clone(),
            period: date,
            period_type,
            statement: q.statement,
            scope: q.scope,
            item: items_dict::dict().normalize(key),
            value: num,
            unit: Unit::Raw,
            currency: currency.clone(),
            publish_date,
            audit: AuditStatus::Unknown,
            source: SourceTag::EastMoney,
        });
    }
    rows
}

/// Filter EM-returned date strings against the user's requested
/// periods. EM step 1 returns every report end it has on file (often
/// 20+ rows); we only fetch step 2 for what was asked.
pub fn filter_dates(all: Vec<String>, requested: &[crate::domain::Period]) -> Vec<String> {
    let allowed: HashSet<Date> = requested.iter().map(|p| p.end_date()).collect();
    all.into_iter()
        .filter(|s| parse_em_date(s).map(|d| allowed.contains(&d)).unwrap_or(false))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::Market;
    use crate::domain::{Period, Scope, Statement};

    fn cn_a_symbol(code: &str) -> Symbol {
        Symbol {
            code: code.into(),
            market: Market::CnA,
        }
    }

    fn cn_a_query() -> Query {
        Query {
            symbol: cn_a_symbol("600519"),
            statement: Statement::Income,
            periods: vec![Period::Annual(2025)],
            scope: Scope::Consolidated,
        }
    }

    #[test]
    fn a_share_code_assigns_sh_sz_bj_correctly() {
        assert_eq!(a_share_code(&cn_a_symbol("600519")), "SH600519");
        assert_eq!(a_share_code(&cn_a_symbol("688123")), "SH688123");
        assert_eq!(a_share_code(&cn_a_symbol("000001")), "SZ000001");
        assert_eq!(a_share_code(&cn_a_symbol("300750")), "SZ300750");
        assert_eq!(a_share_code(&cn_a_symbol("830799")), "BJ830799");
        assert_eq!(a_share_code(&cn_a_symbol("430510")), "BJ430510");
        // B-share split: 900* = SH, 200* = SZ.
        assert_eq!(a_share_code(&cn_a_symbol("900901")), "SH900901");
        assert_eq!(a_share_code(&cn_a_symbol("200012")), "SZ200012");
    }

    #[test]
    fn secucode_appends_market_suffix() {
        let hk = Symbol {
            code: "00700".into(),
            market: Market::Hk,
        };
        assert_eq!(secucode(&hk), "00700.HK");

        let us = Symbol {
            code: "AAPL".into(),
            market: Market::Us,
        };
        assert_eq!(secucode(&us), "AAPL.US");
    }

    #[test]
    fn parse_em_date_accepts_both_iso_and_datetime() {
        let d = parse_em_date("2025-12-31").unwrap();
        assert_eq!(d.year(), 2025);
        assert_eq!(d.month() as u8, 12);
        assert_eq!(d.day(), 31);

        let d2 = parse_em_date("2024-06-30 00:00:00").unwrap();
        assert_eq!(d2.month() as u8, 6);
        assert_eq!(d2.day(), 30);

        assert!(parse_em_date("garbage").is_none());
    }

    #[test]
    fn report_type_label_to_period_type() {
        assert_eq!(report_type_to_period_type("年报"), Some(PeriodType::Annual));
        assert_eq!(report_type_to_period_type("中报"), Some(PeriodType::H1));
        assert_eq!(report_type_to_period_type("半年报"), Some(PeriodType::H1));
        assert_eq!(report_type_to_period_type("一季报"), Some(PeriodType::Q1));
        assert_eq!(report_type_to_period_type("三季报"), Some(PeriodType::Q3));
        assert_eq!(report_type_to_period_type("某种不认识的形态"), None);
    }

    #[test]
    fn a_wide_to_rows_skips_metadata_yoy_and_nulls() {
        let raw = serde_json::json!({
            "SECUCODE": "600519.SH",
            "SECURITY_NAME_ABBR": "贵州茅台",
            "REPORT_DATE": "2025-12-31",
            "REPORT_TYPE": "年报",
            "CURRENCY": "CNY",
            "NOTICE_DATE": "2026-03-30",
            "TOTAL_OPERATE_INCOME": 172054171891.0,
            "TOTAL_OPERATE_INCOME_YOY": 15.6,
            "OPERATE_COST": 14892277571.0,
            "PARENT_NETPROFIT": 82320067102.0,
            "BASIC_EPS": 65.66,
            "SOMETHING_NULL": serde_json::Value::Null
        });
        let entry = raw.as_object().unwrap();
        let rows = a_wide_to_rows(entry, &cn_a_query());

        // 4 non-metadata, non-YOY, non-null fields → 4 rows.
        assert_eq!(rows.len(), 4, "rows: {rows:#?}");

        let row_by_item: std::collections::HashMap<String, &FinancialRow> =
            rows.iter().map(|r| (r.item.clone(), r)).collect();

        // Hits dict.
        assert!(row_by_item.contains_key("营业总收入"));
        assert_eq!(row_by_item["营业总收入"].value, 172054171891.0);
        assert_eq!(row_by_item["营业总收入"].currency, "CNY");
        assert_eq!(row_by_item["营业总收入"].name, "贵州茅台");
        assert_eq!(row_by_item["营业总收入"].period.year(), 2025);
        assert_eq!(row_by_item["营业总收入"].period_type, PeriodType::Annual);

        assert!(row_by_item.contains_key("归母净利润"));
        assert!(row_by_item.contains_key("基本每股收益"));
        assert!(row_by_item.contains_key("营业成本"));
    }

    #[test]
    fn a_wide_to_rows_keeps_unknown_columns_verbatim_and_records_them() {
        // Clear any residual state so this test can assert exactly.
        let _ = items_dict::drain_unmapped();

        let raw = serde_json::json!({
            "REPORT_DATE": "2025-12-31",
            "REPORT_TYPE": "年报",
            "CURRENCY": "CNY",
            "MOCK_COL_XYZ": 42.0,
        });
        let rows = a_wide_to_rows(raw.as_object().unwrap(), &cn_a_query());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].item, "MOCK_COL_XYZ"); // passthrough on miss
        assert_eq!(rows[0].value, 42.0);

        let drained = items_dict::drain_unmapped();
        assert!(
            drained.contains(&"MOCK_COL_XYZ".to_string()),
            "drained: {drained:?}"
        );
    }

    #[test]
    fn filter_dates_keeps_only_requested_period_ends() {
        let all = vec![
            "2025-12-31".to_string(),
            "2025-09-30".to_string(),
            "2025-06-30".to_string(),
            "2025-03-31".to_string(),
            "2024-12-31".to_string(),
        ];
        let requested = vec![Period::Annual(2025), Period::Q1(2025)];
        let kept = filter_dates(all, &requested);
        assert_eq!(kept, vec!["2025-12-31".to_string(), "2025-03-31".to_string()]);
    }
}
