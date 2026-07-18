//! Main financial indicators (`Statement::Indicator`).
//!
//! - A-share: `RPT_F10_FINANCE_MAINFINADATA` — wide table, one row per
//!   `REPORT_DATE`. Field translation reuses the same dictionary as
//!   the three statements.
//! - HK: `RPT_HKF10_FN_MAININDICATOR` — also wide. **Stubbed**
//!   (returns empty); the field set differs significantly and
//!   needs its own dictionary pass.

use serde_json::{Map, Value};
use time::Date;

use crate::domain::{
    items_dict, AuditStatus, FinancialRow, Period, PeriodType, Query, SourceTag, Symbol, Unit,
};
use crate::error::SiftError;
use crate::http::HttpClient;

use super::translate;
use super::EastmoneyFinancialSource;

pub(crate) fn fetch_a(
    src: &EastmoneyFinancialSource,
    q: &Query,
    http: &HttpClient,
) -> Result<Vec<FinancialRow>, SiftError> {
    let secucode = format!(
        "{}.{}",
        q.symbol.code,
        a_secucode_suffix(&q.symbol)
    );
    let url = format!(
        "{base}?reportName=RPT_F10_FINANCE_MAINFINADATA&columns=ALL\
         &filter=(SECUCODE=%22{secucode}%22)\
         &source=HSF10&client=PC",
        base = src.urls().datacenter_base,
    );
    let bytes = http.get_bytes(&url)?;
    let resp: WideLongResp = serde_json::from_slice(&bytes)
        .map_err(|e| SiftError::Internal(format!("eastmoney indicator A parse: {e}")))?;

    let allowed: std::collections::HashSet<Date> =
        q.periods.iter().map(Period::end_date).collect();

    let mut rows = Vec::new();
    for entry in &resp.result.data {
        let Some(date) = entry
            .get("REPORT_DATE")
            .and_then(|v| v.as_str())
            .and_then(translate::parse_em_date)
        else {
            continue;
        };
        if !allowed.is_empty() && !allowed.contains(&date) {
            continue;
        }
        rows.extend(wide_to_indicator_rows(entry, date, q));
    }
    Ok(rows)
}

/// HK indicator stub — see module docs.
pub(crate) fn fetch_hk(
    _src: &EastmoneyFinancialSource,
    _q: &Query,
    _http: &HttpClient,
) -> Result<Vec<FinancialRow>, SiftError> {
    Ok(Vec::new())
}

fn a_secucode_suffix(sym: &Symbol) -> &'static str {
    use crate::domain::market::{infer_board, Board};
    match infer_board(&sym.code) {
        Some(Board::ShMain | Board::ShStar) => "SH",
        Some(Board::BShare) if sym.code.starts_with('9') => "SH",
        Some(_) => "SZ",
        None => "SH",
    }
}

fn wide_to_indicator_rows(
    entry: &Map<String, Value>,
    date: Date,
    q: &Query,
) -> Vec<FinancialRow> {
    let period_type = entry
        .get("REPORT_TYPE")
        .and_then(|v| v.as_str())
        .and_then(translate::report_type_to_period_type)
        .or_else(|| PeriodType::from_date(date))
        .unwrap_or(PeriodType::Annual);
    let currency = entry
        .get("CURRENCY")
        .and_then(|v| v.as_str())
        .unwrap_or("CNY")
        .to_string();
    let name = entry
        .get("SECURITY_NAME_ABBR")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let publish_date = entry
        .get("NOTICE_DATE")
        .and_then(|v| v.as_str())
        .and_then(translate::parse_em_date);

    let mut rows = Vec::new();
    for (key, value) in entry {
        if translate::is_metadata_col(key) {
            continue;
        }
        let Some(num) = translate::extract_number(value) else {
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

#[derive(serde::Deserialize, Default)]
struct WideLongResp {
    #[serde(default)]
    result: WideLongResult,
}

#[derive(serde::Deserialize, Default)]
struct WideLongResult {
    #[serde(default)]
    data: Vec<Map<String, Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::Market;
    use crate::domain::{Period, Scope, Statement};

    #[test]
    fn a_share_indicator_wide_parses_into_rows() {
        let body = r#"{
            "result": {
                "data": [
                    {
                        "SECUCODE": "600519.SH",
                        "SECURITY_NAME_ABBR": "贵州茅台",
                        "REPORT_DATE": "2025-12-31",
                        "REPORT_TYPE": "年报",
                        "CURRENCY": "CNY",
                        "ROE_AVG": 28.5,
                        "ROA_AVG": 17.2,
                        "GROSS_PROFIT_RATIO": 91.3,
                        "NET_PROFIT_RATIO": 49.0,
                        "DEBT_ASSET_RATIO": 18.1,
                        "BPS": 245.7
                    }
                ]
            }
        }"#;
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "reportName".into(),
                "RPT_F10_FINANCE_MAINFINADATA".into(),
            )]))
            .with_status(200)
            .with_body(body)
            .expect(1)
            .create();

        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let q = Query {
            symbol: Symbol {
                code: "600519".into(),
                market: Market::CnA,
                kind: crate::domain::market::InstrumentKind::Equity,
            },
            statement: Statement::Indicator,
            periods: vec![Period::Annual(2025)],
            scope: Scope::Consolidated,
        };
        let rows = fetch_a(&src, &q, &HttpClient::new()).unwrap();
        assert!(rows.len() >= 5, "rows: {rows:#?}");
        for r in &rows {
            assert_eq!(r.statement, Statement::Indicator);
            assert_eq!(r.period.year(), 2025);
            assert_eq!(r.period_type, PeriodType::Annual);
        }
        let items: Vec<&str> = rows.iter().map(|r| r.item.as_str()).collect();
        // EM column names normalize to standardized Chinese names.
        assert!(items.contains(&"ROE"), "items: {items:?}");
        assert!(items.contains(&"ROA"), "items: {items:?}");
        assert!(items.contains(&"毛利率"), "items: {items:?}");
        assert!(items.contains(&"资产负债率"), "items: {items:?}");
    }

    #[test]
    fn hk_indicator_is_stubbed_to_empty() {
        let src = EastmoneyFinancialSource::with_urls("http://unused", "http://unused");
        let q = Query {
            symbol: Symbol {
                code: "00700".into(),
                market: Market::Hk,
                kind: crate::domain::market::InstrumentKind::Equity,
            },
            statement: Statement::Indicator,
            periods: vec![Period::Annual(2024)],
            scope: Scope::Consolidated,
        };
        let rows = fetch_hk(&src, &q, &HttpClient::new()).unwrap();
        assert!(rows.is_empty());
    }
}
