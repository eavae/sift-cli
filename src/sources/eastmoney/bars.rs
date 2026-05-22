//! EM `kline/get` daily K-line integration (F5 story-02).
//!
//! Endpoint: `{base}/api/qt/stock/kline/get` returns plain JSON:
//!
//! ```json
//! {"rc":0,"data":{"code":"600519","name":"贵州茅台","klt":101,"fqt":0,
//!   "klines":[
//!     "2024-01-02,1730.00,1685.00,1750.00,1680.00,123456,9.87e9,3.5,-2.0,-35.0,0.5",
//!     ...
//!   ]}}
//! ```
//!
//! Each `klines[i]` is a comma-separated CSV string in **EM's
//! original order**: `date, open, close, high, low, volume(hands),
//! amount(yuan), amplitude%, pct_change%, change, turnover%` —
//! `open` is followed by `close`, not by `high`. sift reorders the
//! output to standard OHLC (see [`crate::domain::bars`]).
//!
//! CN A-share queries use the `beg`/`end` date range parameters; HK
//! and US use `lmt=1000000` to grab the whole history. The branch
//! is taken inside this module; callers only provide a [`BarsQuery`].

use serde_json::Value;

use crate::domain::bars::{Adjust, BarRow};
use crate::domain::market::Market;
use crate::domain::Symbol;
use crate::error::SiftError;
use crate::http::HttpClient;

use super::secid;

pub const DEFAULT_BARS_BASE: &str = "https://push2his.eastmoney.com";

const UT: &str = "7eea3edcaed734bea9cbfc24409ed989";
/// Daily K-line (README "data source & protocol" §`klt=101`).
const KLT_DAILY: &str = "101";
/// "Grab-all" lmt ceiling. EM accepts arbitrarily large integers;
/// a six-figure value covers any single instrument's history (CN A-
/// share runs ≈ 7,500 trading days over 30+ years; HK similar; long-
/// running US tickers around 15,000 trading days at most).
const LMT_ALL: &str = "1000000";

#[derive(Debug, Clone)]
pub struct EmBarsUrls {
    pub bars_base: String,
}

impl EmBarsUrls {
    pub fn from_env() -> Self {
        Self {
            bars_base: std::env::var("SIFT_EM_BARS_BASE")
                .unwrap_or_else(|_| DEFAULT_BARS_BASE.into()),
        }
    }
}

/// Query parameters. `start` / `end` (ISO dates) are inclusive and
/// applied client-side; `limit` selects the most recent N trading
/// days. The `limit`-vs-`start`/`end` mutual exclusion is enforced
/// by clap in the command layer, so we do not re-validate here.
#[derive(Debug, Clone)]
pub struct BarsQuery {
    pub symbol: Symbol,
    pub start: Option<time::Date>,
    pub end: Option<time::Date>,
    pub limit: Option<usize>,
    pub adjust: Adjust,
}

impl BarsQuery {
    fn fqt(&self) -> &'static str {
        match self.adjust {
            Adjust::None => "0",
            Adjust::Pre => "1",
            Adjust::Post => "2",
        }
    }
}

/// Fetch a symbol's daily K-line series and truncate client-side to
/// the requested range.
pub fn bars_daily_with_base(
    http: &HttpClient,
    q: &BarsQuery,
    base: &str,
) -> Result<Vec<BarRow>, SiftError> {
    let mut url = format!(
        "{base}/api/qt/stock/kline/get?secid={secid}&klt={KLT_DAILY}&fqt={fqt}\
         &fields1=f1,f2,f3,f4,f5,f6\
         &fields2=f51,f52,f53,f54,f55,f56,f57,f58,f59,f60,f61\
         &ut={UT}",
        secid = secid(&q.symbol),
        fqt = q.fqt(),
    );
    // CN A-share uses beg/end for ranges. EM's HK/US endpoints honor
    // beg/end unreliably, so we always use the `lmt` "grab-all"
    // form there and filter client-side.
    match q.symbol.market {
        Market::CnA => {
            let beg = q.start.map(format_yyyymmdd).unwrap_or_else(|| "0".into());
            let end = q.end.map(format_yyyymmdd).unwrap_or_else(|| "20500101".into());
            url.push_str(&format!("&beg={beg}&end={end}"));
        }
        Market::Hk | Market::Us => {
            url.push_str(&format!("&lmt={LMT_ALL}"));
        }
    }

    let bytes = http.get_bytes(&url)?;
    let rows = parse(&bytes, &q.symbol, q.adjust)?;
    Ok(apply_filters(rows, q))
}

fn format_yyyymmdd(d: time::Date) -> String {
    format!(
        "{:04}{:02}{:02}",
        d.year(),
        d.month() as u8,
        d.day(),
    )
}

fn parse(bytes: &[u8], symbol: &Symbol, adjust: Adjust) -> Result<Vec<BarRow>, SiftError> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|e| SiftError::Parse(format!("EM kline/get not JSON: {e}")))?;
    let data = v
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| SiftError::Parse("EM kline/get: missing `data` object".into()))?;

    // Empty klines with no name = EM does not recognize this symbol
    // (returns HTTP 200 but `klines` is an empty array and `name`
    // is missing/empty). Surface as `NotFound` so the command layer
    // can classify and aggregate appropriately.
    let name = data
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    let klines = match data.get("klines") {
        Some(Value::Array(a)) => a,
        Some(Value::Null) | None => {
            return Err(SiftError::NotFound(format!(
                "{}.{}",
                symbol.code,
                symbol.market.as_upper(),
            )));
        }
        other => {
            return Err(SiftError::Parse(format!(
                "EM kline/get: unexpected `klines` type {other:?}",
            )));
        }
    };

    if klines.is_empty() && name.is_none() {
        return Err(SiftError::NotFound(format!(
            "{}.{}",
            symbol.code,
            symbol.market.as_upper(),
        )));
    }

    let display_symbol = format!("{}.{}", symbol.code, symbol.market.as_upper());
    let mut rows: Vec<BarRow> = Vec::with_capacity(klines.len());
    for entry in klines {
        let s = entry.as_str().ok_or_else(|| {
            SiftError::Parse(format!("EM kline entry not a string: {entry:?}"))
        })?;
        rows.push(parse_one(s, &display_symbol, adjust)?);
    }
    Ok(rows)
}

fn parse_one(csv: &str, display_symbol: &str, adjust: Adjust) -> Result<BarRow, SiftError> {
    // EM's raw order:
    //   date, open, close, high, low, volume(hands), amount,
    //   amplitude%, pct_change%, change, turnover%
    let fields: Vec<&str> = csv.split(',').collect();
    if fields.len() < 11 {
        return Err(SiftError::Parse(format!(
            "EM kline row has {} fields, expected ≥11: {csv:?}",
            fields.len()
        )));
    }
    let date = fields[0].to_string();
    let open: f64 = parse_num(fields[1], "open")?;
    let close: f64 = parse_num(fields[2], "close")?;
    let high: f64 = parse_num(fields[3], "high")?;
    let low: f64 = parse_num(fields[4], "low")?;
    let volume_hand: f64 = parse_num(fields[5], "volume")?;
    let amount: f64 = parse_num(fields[6], "amount")?;
    let amplitude_pct: f64 = parse_num(fields[7], "amplitude_pct")?;
    let pct_change: f64 = parse_num(fields[8], "pct_change")?;
    let change: f64 = parse_num(fields[9], "change")?;
    let turnover_pct: f64 = parse_num(fields[10], "turnover_pct")?;

    Ok(BarRow {
        symbol: display_symbol.to_string(),
        date,
        open,
        high,
        low,
        close,
        volume: (volume_hand * 100.0) as i64, // hands → shares
        amount,
        pct_change,
        change,
        amplitude_pct,
        turnover_pct,
        adjust,
        source: "eastmoney",
    })
}

fn parse_num(s: &str, name: &str) -> Result<f64, SiftError> {
    s.trim().parse::<f64>().map_err(|e| {
        SiftError::Parse(format!("EM kline field {name}={s:?} not numeric: {e}"))
    })
}

/// Client-side filtering. `start`/`end` trim the date range (EM
/// occasionally returns rows outside the requested window); `limit`
/// keeps the most recent N rows. EM returns rows in ascending date
/// order, so `limit` is taken from the tail.
fn apply_filters(rows: Vec<BarRow>, q: &BarsQuery) -> Vec<BarRow> {
    let mut out: Vec<BarRow> = rows;
    if q.start.is_some() || q.end.is_some() {
        out.retain(|r| match parse_iso_date(&r.date) {
            Some(d) => {
                q.start.map(|s| d >= s).unwrap_or(true)
                    && q.end.map(|e| d <= e).unwrap_or(true)
            }
            None => false,
        });
    }
    if let Some(n) = q.limit {
        if out.len() > n {
            let drop = out.len() - n;
            out.drain(0..drop);
        }
    }
    out
}

fn parse_iso_date(s: &str) -> Option<time::Date> {
    use time::format_description::well_known::Iso8601;
    time::Date::parse(s, &Iso8601::DATE).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cn_a(code: &str) -> Symbol {
        Symbol {
            code: code.into(),
            market: Market::CnA,
        }
    }

    fn hk(code: &str) -> Symbol {
        Symbol {
            code: code.into(),
            market: Market::Hk,
        }
    }

    fn em_body(klines: &[&str]) -> String {
        let arr = klines
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"rc":0,"data":{{"code":"600519","name":"贵州茅台","klt":101,"klines":[{arr}]}}}}"#,
        )
    }

    #[test]
    fn ochl_is_reordered_to_ohlc() {
        // EM raw: date, open=10, close=15, high=20, low=8, vol=100,
        //         amount=1000, amplitude=1, pct=50, change=5, turnover=0.1
        let body = em_body(&["2024-01-02,10,15,20,8,100,1000,1,50,5,0.1"]);
        let rows = parse(body.as_bytes(), &cn_a("600519"), Adjust::None).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.symbol, "600519.CN-A");
        assert_eq!(r.date, "2024-01-02");
        assert!((r.open - 10.0).abs() < 1e-9);
        assert!((r.high - 20.0).abs() < 1e-9);
        assert!((r.low - 8.0).abs() < 1e-9);
        assert!((r.close - 15.0).abs() < 1e-9);
        // hands → shares: 100 × 100 = 10000
        assert_eq!(r.volume, 10_000);
        assert!((r.amount - 1000.0).abs() < 1e-9);
        assert!((r.pct_change - 50.0).abs() < 1e-9);
        assert!((r.change - 5.0).abs() < 1e-9);
        assert!((r.amplitude_pct - 1.0).abs() < 1e-9);
        assert!((r.turnover_pct - 0.1).abs() < 1e-9);
        assert_eq!(r.adjust, Adjust::None);
    }

    #[test]
    fn empty_klines_yields_not_found() {
        let body = r#"{"rc":0,"data":{"klines":[]}}"#;
        let err = parse(body.as_bytes(), &cn_a("999999"), Adjust::None).unwrap_err();
        assert!(matches!(err, SiftError::NotFound(_)), "got {err:?}");
    }

    #[test]
    fn malformed_csv_yields_parse_error() {
        let body = em_body(&["2024-01-02,10,15,20"]); // only 4 fields
        let err = parse(body.as_bytes(), &cn_a("600519"), Adjust::None).unwrap_err();
        assert!(matches!(err, SiftError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn limit_truncates_oldest_rows() {
        let rows = vec![bar_at("2024-01-02"), bar_at("2024-01-03"), bar_at("2024-01-04")];
        let q = BarsQuery {
            symbol: cn_a("600519"),
            start: None,
            end: None,
            limit: Some(2),
            adjust: Adjust::None,
        };
        let filtered = apply_filters(rows, &q);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].date, "2024-01-03");
        assert_eq!(filtered[1].date, "2024-01-04");
    }

    #[test]
    fn date_range_filters_inclusive() {
        let rows = vec![
            bar_at("2024-01-02"),
            bar_at("2024-01-03"),
            bar_at("2024-01-04"),
        ];
        let q = BarsQuery {
            symbol: cn_a("600519"),
            start: Some(time::Date::from_calendar_date(2024, time::Month::January, 3).unwrap()),
            end: Some(time::Date::from_calendar_date(2024, time::Month::January, 3).unwrap()),
            limit: None,
            adjust: Adjust::None,
        };
        let filtered = apply_filters(rows, &q);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].date, "2024-01-03");
    }

    fn bar_at(date: &str) -> BarRow {
        BarRow {
            symbol: "600519.CN-A".into(),
            date: date.into(),
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1,
            amount: 1.0,
            pct_change: 0.0,
            change: 0.0,
            amplitude_pct: 0.0,
            turnover_pct: 0.0,
            adjust: Adjust::None,
            source: "eastmoney",
        }
    }

    #[test]
    fn cn_a_uses_beg_end_params() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/api/qt/stock/kline/get")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("secid".into(), "1.600519".into()),
                mockito::Matcher::UrlEncoded("beg".into(), "20240101".into()),
                mockito::Matcher::UrlEncoded("end".into(), "20240131".into()),
            ]))
            .with_status(200)
            .with_body(em_body(&["2024-01-15,10,15,20,8,100,1000,1,2,3,0.1"]))
            .expect(1)
            .create();
        let q = BarsQuery {
            symbol: cn_a("600519"),
            start: time::Date::from_calendar_date(2024, time::Month::January, 1).ok(),
            end: time::Date::from_calendar_date(2024, time::Month::January, 31).ok(),
            limit: None,
            adjust: Adjust::Pre,
        };
        let rows = bars_daily_with_base(&HttpClient::new(), &q, &server.url()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].adjust, Adjust::Pre);
        m.assert();
    }

    #[test]
    fn hk_uses_lmt_instead_of_beg_end() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/api/qt/stock/kline/get")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("secid".into(), "116.00700".into()),
                mockito::Matcher::UrlEncoded("lmt".into(), "1000000".into()),
            ]))
            .with_status(200)
            .with_body(em_body(&["2024-01-15,500,510,520,490,100,1000,1,2,3,0.1"]))
            .expect(1)
            .create();
        let q = BarsQuery {
            symbol: hk("00700"),
            start: None,
            end: None,
            limit: None,
            adjust: Adjust::None,
        };
        let rows = bars_daily_with_base(&HttpClient::new(), &q, &server.url()).unwrap();
        assert_eq!(rows.len(), 1);
        m.assert();
    }
}
