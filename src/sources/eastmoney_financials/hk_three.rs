//! Hong Kong three statements via the `datacenter v1/get` long table.
//!
//! Two HTTP calls per request:
//!
//! 1. **Summary** (`RPT_CUSTOM_HKSK_APPFN_CASHFLOW_SUMMARY`) → resolve
//!    `(period_type, currency, publish_date)` per `REPORT_DATE`. If
//!    this call fails, the long-table call is still attempted and the
//!    metadata is filled in from the `DATE_TYPE_CODE` field on the
//!    long row (or defaults).
//! 2. **Long table** (`RPT_HKF10_FN_INCOME_PC` / `_BALANCE_PC` /
//!    `_CASHFLOW_PC`) → one JSON row per `(REPORT_DATE, item)` tuple.

use std::collections::HashMap;

use time::Date;

use crate::domain::{
    items_dict, AuditStatus, FinancialRow, Period, PeriodType, Query, Scope, SourceTag, Statement,
    Symbol, Unit,
};
use crate::error::SiftError;
use crate::http::HttpClient;

use super::translate;
use super::EastmoneyFinancialSource;

/// Per-`REPORT_DATE` metadata pulled from the summary endpoint.
#[derive(Debug, Default, Clone)]
struct RowMeta {
    period_type: Option<PeriodType>,
    currency: Option<String>,
    publish_date: Option<Date>,
}

pub(crate) fn fetch(
    src: &EastmoneyFinancialSource,
    q: &Query,
    http: &HttpClient,
) -> Result<Vec<FinancialRow>, SiftError> {
    let meta_by_date = fetch_meta(src, q, http).unwrap_or_default();

    let long_url = build_long_url(&src.urls().datacenter_base, q);
    let bytes = http.get_bytes(&long_url)?;
    let resp: LongResp = serde_json::from_slice(&bytes)
        .map_err(|e| SiftError::Internal(format!("eastmoney HK long parse: {e}")))?;

    let allowed_dates: std::collections::HashSet<Date> =
        q.periods.iter().map(Period::end_date).collect();

    let mut rows = Vec::new();
    for entry in &resp.result.data {
        let Some(date_str) = entry.get("REPORT_DATE").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(date) = translate::parse_em_date(date_str) else {
            continue;
        };
        if !allowed_dates.is_empty() && !allowed_dates.contains(&date) {
            continue;
        }
        let Some(item_label) = entry.get("STD_ITEM_NAME").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(value) = entry.get("AMOUNT").and_then(translate::extract_number) else {
            continue;
        };

        // Fallback chain for metadata:
        //   1. Summary endpoint join (preferred).
        //   2. `DATE_TYPE_CODE` on the long row (`Period::from_date_type_code`).
        //   3. Date inference (`PeriodType::from_date`).
        let meta = meta_by_date.get(&date).cloned().unwrap_or_default();
        let period_type = meta
            .period_type
            .or_else(|| {
                entry
                    .get("DATE_TYPE_CODE")
                    .and_then(|v| v.as_str())
                    .and_then(|c| Period::from_date_type_code(c, date).ok())
                    .and_then(|p| p.period_type())
            })
            .or_else(|| PeriodType::from_date(date))
            .unwrap_or(PeriodType::Annual);

        let currency = meta
            .currency
            .clone()
            .or_else(|| {
                entry
                    .get("CURRENCY")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_default();

        let publish_date = meta.publish_date.or_else(|| {
            entry
                .get("NOTICE_DATE")
                .and_then(|v| v.as_str())
                .and_then(translate::parse_em_date)
        });

        let name = entry
            .get("SECURITY_NAME_ABBR")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        rows.push(FinancialRow {
            symbol: q.symbol.clone(),
            name,
            period: date,
            period_type,
            statement: q.statement,
            scope: Scope::Consolidated,
            item: items_dict::dict().normalize(item_label),
            value,
            unit: Unit::Raw,
            currency,
            publish_date,
            audit: AuditStatus::Unknown,
            source: SourceTag::EastMoney,
        });
    }
    Ok(rows)
}

fn fetch_meta(
    src: &EastmoneyFinancialSource,
    q: &Query,
    http: &HttpClient,
) -> Result<HashMap<Date, RowMeta>, SiftError> {
    let url = build_summary_url(&src.urls().datacenter_base, &q.symbol);
    let bytes = http.get_bytes(&url)?;
    let resp: LongResp = serde_json::from_slice(&bytes)
        .map_err(|e| SiftError::Internal(format!("eastmoney HK summary parse: {e}")))?;

    let mut out: HashMap<Date, RowMeta> = HashMap::new();
    for entry in &resp.result.data {
        let Some(date_str) = entry.get("REPORT_DATE").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(date) = translate::parse_em_date(date_str) else {
            continue;
        };
        let period_type = entry
            .get("REPORT_TYPE")
            .and_then(|v| v.as_str())
            .and_then(translate::report_type_to_period_type);
        let currency = entry
            .get("CURRENCY")
            .and_then(|v| v.as_str())
            .map(String::from);
        let publish_date = entry
            .get("NOTICE_DATE")
            .and_then(|v| v.as_str())
            .and_then(translate::parse_em_date);
        out.insert(
            date,
            RowMeta {
                period_type,
                currency,
                publish_date,
            },
        );
    }
    Ok(out)
}

fn build_summary_url(base: &str, sym: &Symbol) -> String {
    let secucode = translate::secucode(sym);
    format!(
        "{base}?reportName=RPT_CUSTOM_HKSK_APPFN_CASHFLOW_SUMMARY\
         &columns=SECUCODE,REPORT_DATE,FISCAL_YEAR,CURRENCY,ACCOUNT_STANDARD,REPORT_TYPE,NOTICE_DATE\
         &filter=(SECUCODE=%22{secucode}%22)\
         &source=F10&client=PC"
    )
}

fn build_long_url(base: &str, q: &Query) -> String {
    let secucode = translate::secucode(&q.symbol);
    let report_name = match q.statement {
        Statement::Balance => "RPT_HKF10_FN_BALANCE_PC",
        Statement::Income => "RPT_HKF10_FN_INCOME_PC",
        Statement::Cashflow => "RPT_HKF10_FN_CASHFLOW_PC",
        Statement::Indicator => unreachable!("Indicator routes to indicator.rs"),
    };
    // ureq 3's `http::Uri` parser rejects raw `"` in the query
    // string (per RFC 3986), so the SECUCODE quotes are
    // percent-encoded as `%22`. Parens are tolerated and stay
    // literal for readability.
    format!(
        "{base}?reportName={report_name}&columns=ALL\
         &filter=(SECUCODE=%22{secucode}%22)\
         &source=F10&client=PC"
    )
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct LongResp {
    #[serde(default)]
    result: LongResult,
}

#[derive(serde::Deserialize, Default)]
struct LongResult {
    #[serde(default)]
    data: Vec<serde_json::Map<String, serde_json::Value>>,
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

    fn tencent_sym() -> Symbol {
        Symbol {
            code: "00700".into(),
            market: Market::Hk,
        }
    }

    fn balance_query() -> Query {
        Query {
            symbol: tencent_sym(),
            statement: Statement::Balance,
            periods: vec![Period::Annual(2024)],
            scope: Scope::Consolidated,
        }
    }

    /// Single mockito server answers BOTH the summary endpoint and the
    /// long-table endpoint; differ by `reportName` in the query string.
    fn mock_meta_and_long(
        server: &mut mockito::Server,
        summary_body: &str,
        long_body: &str,
    ) -> (mockito::Mock, mockito::Mock) {
        let m_summary = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "reportName".into(),
                "RPT_CUSTOM_HKSK_APPFN_CASHFLOW_SUMMARY".into(),
            )]))
            .with_status(200)
            .with_body(summary_body)
            .expect(1)
            .create();
        let m_long = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "reportName".into(),
                "RPT_HKF10_FN_BALANCE_PC".into(),
            )]))
            .with_status(200)
            .with_body(long_body)
            .expect(1)
            .create();
        (m_summary, m_long)
    }

    #[test]
    fn hk_long_table_joins_summary_metadata() {
        let summary = r#"{
            "result": {
                "data": [
                    {
                        "REPORT_DATE": "2024-12-31 00:00:00",
                        "CURRENCY": "人民币",
                        "REPORT_TYPE": "年报",
                        "NOTICE_DATE": "2025-03-21 00:00:00"
                    }
                ]
            }
        }"#;
        let long = r#"{
            "result": {
                "data": [
                    {
                        "REPORT_DATE": "2024-12-31 00:00:00",
                        "SECUCODE": "00700.HK",
                        "SECURITY_NAME_ABBR": "腾讯控股",
                        "STD_ITEM_NAME": "资产总计",
                        "AMOUNT": 1885712000000.0
                    },
                    {
                        "REPORT_DATE": "2024-12-31 00:00:00",
                        "SECUCODE": "00700.HK",
                        "SECURITY_NAME_ABBR": "腾讯控股",
                        "STD_ITEM_NAME": "负债合计",
                        "AMOUNT": 723000000000.0
                    }
                ]
            }
        }"#;
        let mut server = mockito::Server::new();
        let (m_sum, m_long) = mock_meta_and_long(&mut server, summary, long);

        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let rows = src.fetch(&balance_query(), &HttpClient::new()).unwrap();
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(r.symbol.code, "00700");
            assert_eq!(r.period.year(), 2024);
            assert_eq!(r.period_type, PeriodType::Annual);
            assert_eq!(r.currency, "人民币");
            assert_eq!(r.scope, Scope::Consolidated);
            assert_eq!(r.source, SourceTag::EastMoney);
            assert_eq!(r.name, "腾讯控股");
        }
        let items: Vec<&str> = rows.iter().map(|r| r.item.as_str()).collect();
        assert!(items.contains(&"资产总计"));
        assert!(items.contains(&"负债合计"));
        m_sum.assert();
        m_long.assert();
    }

    #[test]
    fn hk_long_table_works_when_summary_fails() {
        let long = r#"{
            "result": {
                "data": [
                    {
                        "REPORT_DATE": "2024-06-30 00:00:00",
                        "SECUCODE": "00700.HK",
                        "STD_ITEM_NAME": "资产总计",
                        "DATE_TYPE_CODE": "002",
                        "CURRENCY": "人民币",
                        "AMOUNT": 1000000.0
                    }
                ]
            }
        }"#;
        let mut server = mockito::Server::new();
        let _m_summary_fail = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "reportName".into(),
                "RPT_CUSTOM_HKSK_APPFN_CASHFLOW_SUMMARY".into(),
            )]))
            .with_status(500)
            .expect_at_least(1)
            .create();
        let _m_long = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "reportName".into(),
                "RPT_HKF10_FN_BALANCE_PC".into(),
            )]))
            .with_status(200)
            .with_body(long)
            .expect(1)
            .create();

        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let q = Query {
            // Query a half-year period to match the fixture's REPORT_DATE.
            periods: vec![Period::H1(2024)],
            ..balance_query()
        };
        let rows = src.fetch(&q, &HttpClient::new()).unwrap();
        // The summary call failed, so meta_by_date is empty; metadata
        // falls back to the long-row `DATE_TYPE_CODE` and `CURRENCY`.
        assert_eq!(rows.len(), 1, "rows: {rows:#?}");
        assert_eq!(rows[0].period_type, PeriodType::H1);
        assert_eq!(rows[0].currency, "人民币");
    }

    #[test]
    fn hk_filters_to_requested_periods() {
        let long = r#"{
            "result": {
                "data": [
                    {"REPORT_DATE":"2024-12-31 00:00:00","STD_ITEM_NAME":"资产总计","AMOUNT":1.0},
                    {"REPORT_DATE":"2023-12-31 00:00:00","STD_ITEM_NAME":"资产总计","AMOUNT":2.0},
                    {"REPORT_DATE":"2022-12-31 00:00:00","STD_ITEM_NAME":"资产总计","AMOUNT":3.0}
                ]
            }
        }"#;
        let summary = r#"{"result":{"data":[]}}"#;
        let mut server = mockito::Server::new();
        let _m_sum = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "reportName".into(),
                "RPT_CUSTOM_HKSK_APPFN_CASHFLOW_SUMMARY".into(),
            )]))
            .with_status(200)
            .with_body(summary)
            .expect(1)
            .create();
        let _m_long = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::AllOf(vec![mockito::Matcher::UrlEncoded(
                "reportName".into(),
                "RPT_HKF10_FN_BALANCE_PC".into(),
            )]))
            .with_status(200)
            .with_body(long)
            .expect(1)
            .create();
        let src = EastmoneyFinancialSource::with_urls(server.url(), server.url());
        let q = Query {
            periods: vec![Period::Annual(2024), Period::Annual(2023)],
            ..balance_query()
        };
        let rows = src.fetch(&q, &HttpClient::new()).unwrap();
        assert_eq!(rows.len(), 2, "rows: {rows:#?}");
        let years: std::collections::HashSet<i32> =
            rows.iter().map(|r| r.period.year()).collect();
        assert!(years.contains(&2024));
        assert!(years.contains(&2023));
        assert!(!years.contains(&2022));
    }
}
