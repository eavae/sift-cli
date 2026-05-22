//! Tencent `ifzq.gtimg.cn` daily / weekly / monthly K-line.
//!
//! Endpoint: `{base}/appstock/app/{path}/get?param={code},{period},
//!           {start},{end},{n_recent},{adj}`
//!
//! - `{path}` = `fqkline` (A-share) / `hkfqkline` (HK) / `usfqkline` (US)
//! - `{code}` = `sh600519` / `hk00700` / `us.AAPL`
//! - `{period}` = `day` / `week` / `month`
//! - `{start}` / `{end}` ISO date or empty
//! - `{n_recent}` integer — when `start`/`end` are blank, take the
//!   last N rows. Tencent caps the response at ~640 rows when both
//!   the recent-N path and the range path are blank; for our range
//!   form we pass `5` as a default tail anchor (it's ignored by
//!   Tencent when start/end are present).
//! - `{adj}` = `qfq` (pre) / `hfq` (post) / `bfq` or empty (none)
//!
//! Response shape:
//!
//! ```json
//! {"code":0,"msg":"","data":{"sh600519":{
//!     "qfqday": [
//!       ["2024-12-25","1487.243","1478.443","1487.243","1474.543","17123.000"],
//!       ...
//!     ],
//!     "qt": { ... }
//! }}}
//! ```
//!
//! For HK each row has a dividend metadata object at index 6 plus
//! optional trailing fields; for US there are extra trailing
//! numeric fields. We only ever consume the first 6 positional
//! fields (`date, open, close, high, low, volume`) and ignore the
//! rest — the same parser works for all three markets.
//!
//! Tencent only reports OCHL + volume. sift's unified `BarRow`
//! also requires `amount`, `pct_change`, `change`, and
//! `amplitude_pct`; the parser computes those client-side:
//!
//! - `amount` ≈ `close × volume_shares` (yuan). Approximate vs.
//!   the broker-reported notional sum (within ~0.5%); good enough
//!   for screening / scripts.
//! - `pct_change` and `change` use the previous row's close as the
//!   reference. For the first row in the returned slice the
//!   previous close is unknown, so both fields are 0.0 — callers
//!   that care about this should request one extra row.
//! - `amplitude_pct` = `(high - low) / prev_close × 100`. Same
//!   caveat as above for the first row.

use serde_json::Value;

use crate::domain::bars::{Adjust, BarRow, BarsQuery, Period};
use crate::error::SiftError;
use crate::http::HttpClient;
use crate::sources::bars_source::BarsSource;

use super::{kline_path, tencent_code};

pub const DEFAULT_TENCENT_BARS_BASE: &str = "https://web.ifzq.gtimg.cn";

/// Tencent bars source — one instance per process; holds the
/// `bars_base` URL so mockito tests can inject custom addresses.
#[derive(Debug, Clone)]
pub struct TencentBarsSource {
    pub bars_base: String,
}

impl TencentBarsSource {
    pub fn from_env() -> Self {
        Self {
            bars_base: std::env::var("SIFT_TENCENT_BARS_BASE")
                .unwrap_or_else(|_| DEFAULT_TENCENT_BARS_BASE.into()),
        }
    }

    #[cfg(test)]
    pub fn with_base(base: impl Into<String>) -> Self {
        Self {
            bars_base: base.into(),
        }
    }
}

/// `fetch::bars` calls this to build the registered source list in
/// `main::run_bars` — mirrors `eastmoney::bars::build`.
pub fn build() -> Box<dyn BarsSource> {
    Box::new(TencentBarsSource::from_env())
}

impl BarsSource for TencentBarsSource {
    fn name(&self) -> &'static str {
        "tencent"
    }

    fn fetch(&self, q: &BarsQuery, http: &HttpClient) -> Result<Vec<BarRow>, SiftError> {
        bars_with_base(http, q, &self.bars_base)
    }
}

/// Tencent `period` token: `day` / `week` / `month`.
fn period_token(period: Period) -> &'static str {
    match period {
        Period::Daily => "day",
        Period::Weekly => "week",
        Period::Monthly => "month",
    }
}

/// Tencent adjustment token: `qfq` / `hfq` / `bfq`. The empty
/// string and `bfq` are equivalent (no adjustment).
fn adjust_token(adjust: Adjust) -> &'static str {
    match adjust {
        Adjust::None => "bfq",
        Adjust::Pre => "qfq",
        Adjust::Post => "hfq",
    }
}

/// Key in the response `data.{code}` map. The literal mirrors the
/// adjust token plus the period token — Tencent uses `qfqday` /
/// `hfqweek` / `bfqmonth` etc. as the JSON key.
fn response_key(adjust: Adjust, period: Period) -> String {
    format!("{}{}", adjust_token(adjust), period_token(period))
}

/// Fetch a symbol's K-line series and truncate client-side.
/// `bars_with_base` is the test seam — production code goes
/// through [`TencentBarsSource::fetch`], which routes here with the
/// source's stored base URL.
pub fn bars_with_base(
    http: &HttpClient,
    q: &BarsQuery,
    base: &str,
) -> Result<Vec<BarRow>, SiftError> {
    let code = tencent_code(&q.symbol);
    let path = kline_path(&q.symbol);
    let beg = q.start.map(format_iso).unwrap_or_default();
    let end = q.end.map(format_iso).unwrap_or_default();
    // n_recent only matters when both beg / end are blank, but
    // Tencent always wants the slot present in the `param` tuple.
    // 640 is the documented response cap; using a smaller value
    // when --limit is set saves a few KB.
    let n_recent = q.limit.unwrap_or(640);

    let url = format!(
        "{base}/appstock/app/{path}/get?param={code},{period},{beg},{end},{n_recent},{adj}",
        period = period_token(q.period),
        adj = adjust_token(q.adjust),
    );
    let bytes = http.get_bytes(&url)?;
    let rows = parse(&bytes, q)?;
    Ok(apply_filters(rows, q))
}

fn format_iso(d: time::Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        d.year(),
        d.month() as u8,
        d.day(),
    )
}

fn parse(bytes: &[u8], q: &BarsQuery) -> Result<Vec<BarRow>, SiftError> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|e| SiftError::Parse(format!("Tencent kline not JSON: {e}")))?;

    let code = tencent_code(&q.symbol);
    let period_str = period_token(q.period);
    let series = v
        .get("data")
        .and_then(|d| d.get(&code))
        .and_then(|c| {
            // Try the requested adjust/period key first; fall back
            // to the unadjusted key Tencent sometimes uses when the
            // ticker has no adjustment history.
            c.get(response_key(q.adjust, q.period))
                .or_else(|| c.get(format!("bfq{}", period_str)))
                .or_else(|| c.get(period_str))
        })
        .and_then(Value::as_array);

    let series = match series {
        Some(s) if !s.is_empty() => s,
        _ => {
            return Err(SiftError::NotFound(format!(
                "{}.{}",
                q.symbol.code,
                q.symbol.market.as_upper()
            )));
        }
    };

    let display_symbol = format!("{}.{}", q.symbol.code, q.symbol.market.as_upper());
    let mut rows: Vec<BarRow> = Vec::with_capacity(series.len());
    let mut prev_close: Option<f64> = None;
    for entry in series {
        let cols = entry.as_array().ok_or_else(|| {
            SiftError::Parse(format!("Tencent kline row not an array: {entry:?}"))
        })?;
        if cols.len() < 6 {
            return Err(SiftError::Parse(format!(
                "Tencent kline row has {} fields, expected ≥6: {cols:?}",
                cols.len()
            )));
        }
        let date = cols[0]
            .as_str()
            .ok_or_else(|| SiftError::Parse(format!("Tencent date not string: {:?}", cols[0])))?
            .to_string();
        let open = parse_num(&cols[1], "open")?;
        let close = parse_num(&cols[2], "close")?;
        let high = parse_num(&cols[3], "high")?;
        let low = parse_num(&cols[4], "low")?;
        let volume_hand = parse_num(&cols[5], "volume")?;

        let volume = (volume_hand * 100.0) as i64; // hands → shares
        let amount = close * (volume as f64); // approximate, see module doc

        // Diff fields use the prior row's close. The very first row
        // in the returned slice has no predecessor so we leave both
        // fields at 0.0.
        let (pct_change, change, amplitude_pct) = match prev_close {
            Some(pc) if pc.abs() > 1e-9 => (
                (close - pc) / pc * 100.0,
                close - pc,
                (high - low) / pc * 100.0,
            ),
            _ => (0.0, 0.0, 0.0),
        };

        rows.push(BarRow {
            symbol: display_symbol.clone(),
            date,
            open,
            high,
            low,
            close,
            volume,
            amount,
            pct_change,
            change,
            amplitude_pct,
            adjust: q.adjust,
            period: q.period,
            source: "tencent",
        });
        prev_close = Some(close);
    }
    Ok(rows)
}

fn parse_num(v: &Value, name: &str) -> Result<f64, SiftError> {
    match v {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| SiftError::Parse(format!("Tencent {name} not f64: {n:?}"))),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|e| SiftError::Parse(format!("Tencent {name}={s:?} not numeric: {e}"))),
        other => Err(SiftError::Parse(format!(
            "Tencent {name} unexpected type: {other:?}"
        ))),
    }
}

/// Client-side filtering — same shape as the EM source's. Tencent
/// supports beg/end in the URL but it also occasionally returns
/// extras, and the `limit`-without-range path needs to drop the
/// oldest rows here.
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
    use crate::domain::market::Market;
    use crate::domain::Symbol;

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

    fn body_a_share(rows: &[&str]) -> String {
        // A-share rows have 6 columns.
        let arr = rows.iter().map(|r| format!("[{r}]")).collect::<Vec<_>>().join(",");
        format!(
            r#"{{"code":0,"msg":"","data":{{"sh600519":{{"qfqday":[{arr}],"qt":{{}}}}}}}}"#,
        )
    }

    fn body_hk(rows: &[&str]) -> String {
        // HK rows include a dividend metadata object at index 6 +
        // sometimes trailing numeric fields.
        let arr = rows.iter().map(|r| format!("[{r}]")).collect::<Vec<_>>().join(",");
        format!(
            r#"{{"code":0,"msg":"","data":{{"hk00700":{{"qfqday":[{arr}],"qt":{{}}}}}}}}"#,
        )
    }

    #[test]
    fn parses_a_share_with_ochl_reorder() {
        // Tencent raw row: date, open, close, high, low, volume(hands)
        let body = body_a_share(&[
            r#""2024-01-02","10","15","20","8","100""#,
            r#""2024-01-03","15","18","19","14","200""#,
        ]);
        let q = BarsQuery {
            symbol: cn_a("600519"),
            start: None,
            end: None,
            limit: None,
            adjust: Adjust::Pre,
            period: Period::Daily,
        };
        let rows = parse(body.as_bytes(), &q).unwrap();
        assert_eq!(rows.len(), 2);

        let r0 = &rows[0];
        assert_eq!(r0.symbol, "600519.CN-A");
        assert_eq!(r0.date, "2024-01-02");
        assert!((r0.open - 10.0).abs() < 1e-9);
        assert!((r0.high - 20.0).abs() < 1e-9);
        assert!((r0.low - 8.0).abs() < 1e-9);
        assert!((r0.close - 15.0).abs() < 1e-9);
        assert_eq!(r0.volume, 10_000); // hands → shares
        // amount ≈ close × volume_shares = 15 × 10_000 = 150_000
        assert!((r0.amount - 150_000.0).abs() < 1e-9);
        // First row: no prev_close, diff fields are 0
        assert_eq!(r0.pct_change, 0.0);
        assert_eq!(r0.change, 0.0);
        assert_eq!(r0.amplitude_pct, 0.0);
        assert_eq!(r0.source, "tencent");
        assert_eq!(r0.adjust, Adjust::Pre);
        assert_eq!(r0.period, Period::Daily);

        let r1 = &rows[1];
        // pct_change: (18 - 15) / 15 * 100 = 20
        assert!((r1.pct_change - 20.0).abs() < 1e-9);
        // change: 18 - 15 = 3
        assert!((r1.change - 3.0).abs() < 1e-9);
        // amplitude_pct: (19 - 14) / 15 * 100 = 33.33...
        assert!((r1.amplitude_pct - (5.0 / 15.0 * 100.0)).abs() < 1e-9);
    }

    #[test]
    fn parses_hk_row_with_extra_fields_ignored() {
        // HK row has trailing dividend metadata + numeric fields.
        let body = body_hk(&[
            r#""2024-12-23","420.8","410.4","421.2","406.4","20856154",{"cqr":"","FHcontent":""},"0.230","878997.643""#,
        ]);
        let q = BarsQuery {
            symbol: hk("00700"),
            start: None,
            end: None,
            limit: None,
            adjust: Adjust::Pre,
            period: Period::Daily,
        };
        let rows = parse(body.as_bytes(), &q).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.symbol, "00700.HK");
        assert!((r.open - 420.8).abs() < 1e-9);
        // OCHL: cols[2] = close = 410.4
        assert!((r.close - 410.4).abs() < 1e-9);
        assert!((r.high - 421.2).abs() < 1e-9);
        assert!((r.low - 406.4).abs() < 1e-9);
    }

    #[test]
    fn period_tokens_are_correct() {
        assert_eq!(period_token(Period::Daily), "day");
        assert_eq!(period_token(Period::Weekly), "week");
        assert_eq!(period_token(Period::Monthly), "month");
    }

    #[test]
    fn adjust_tokens_are_correct() {
        assert_eq!(adjust_token(Adjust::None), "bfq");
        assert_eq!(adjust_token(Adjust::Pre), "qfq");
        assert_eq!(adjust_token(Adjust::Post), "hfq");
    }

    #[test]
    fn response_key_composes_adjust_and_period() {
        assert_eq!(response_key(Adjust::Pre, Period::Weekly), "qfqweek");
        assert_eq!(response_key(Adjust::None, Period::Monthly), "bfqmonth");
    }

    #[test]
    fn source_trait_routes_to_bars_with_base() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/appstock/app/fqkline/get")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(body_a_share(&[r#""2024-01-02","10","15","20","8","100""#]))
            .expect(1)
            .create();
        let src = TencentBarsSource::with_base(server.url());
        assert_eq!(src.name(), "tencent");
        let q = BarsQuery {
            symbol: cn_a("600519"),
            start: None,
            end: None,
            limit: Some(1),
            adjust: Adjust::Pre,
            period: Period::Daily,
        };
        let rows = src.fetch(&q, &HttpClient::new()).unwrap();
        assert_eq!(rows.len(), 1);
        m.assert();
    }

    #[test]
    fn empty_series_yields_not_found() {
        let body = r#"{"code":0,"msg":"","data":{"sh600519":{"qfqday":[]}}}"#;
        let q = BarsQuery {
            symbol: cn_a("600519"),
            start: None,
            end: None,
            limit: None,
            adjust: Adjust::Pre,
            period: Period::Daily,
        };
        let err = parse(body.as_bytes(), &q).unwrap_err();
        assert!(matches!(err, SiftError::NotFound(_)), "got {err:?}");
    }

    #[test]
    fn malformed_row_yields_parse_error() {
        let body = body_a_share(&[r#""2024-01-02","10","15","20""#]); // 4 fields
        let q = BarsQuery {
            symbol: cn_a("600519"),
            start: None,
            end: None,
            limit: None,
            adjust: Adjust::Pre,
            period: Period::Daily,
        };
        let err = parse(body.as_bytes(), &q).unwrap_err();
        assert!(matches!(err, SiftError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn end_to_end_against_mockito() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/appstock/app/fqkline/get")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded(
                    "param".into(),
                    "sh600519,day,2024-01-01,2024-01-31,5,qfq".into(),
                ),
            ]))
            .with_status(200)
            .with_body(body_a_share(&[
                r#""2024-01-02","10","15","20","8","100""#,
            ]))
            .expect(1)
            .create();

        let q = BarsQuery {
            symbol: cn_a("600519"),
            start: time::Date::from_calendar_date(2024, time::Month::January, 1).ok(),
            end: time::Date::from_calendar_date(2024, time::Month::January, 31).ok(),
            limit: Some(5),
            adjust: Adjust::Pre,
            period: Period::Daily,
        };
        let rows = bars_with_base(&HttpClient::new(), &q, &server.url()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "tencent");
        m.assert();
    }

    #[test]
    fn hk_uses_hkfqkline_path() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/appstock/app/hkfqkline/get")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(body_hk(&[
                r#""2024-12-23","420.8","410.4","421.2","406.4","20856154""#,
            ]))
            .expect(1)
            .create();

        let q = BarsQuery {
            symbol: hk("00700"),
            start: None,
            end: None,
            limit: Some(1),
            adjust: Adjust::Pre,
            period: Period::Daily,
        };
        let rows = bars_with_base(&HttpClient::new(), &q, &server.url()).unwrap();
        assert_eq!(rows.len(), 1);
        m.assert();
    }
}
