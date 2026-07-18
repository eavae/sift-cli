//! EM `stock/get` current-price integration.
//!
//! Endpoint: `{base}/api/qt/stock/get?secid=…&fields=…&ut=…`
//! Returns plain JSON (not JSONP — the push2 snapshot endpoint
//! dropped the JSONP wrapper):
//!
//! ```json
//! { "rc":0, "rt":4, "data": {
//!     "f43":132359, "f44":133299, "f45":132000, "f46":132100,
//!     "f47":1267432, "f48":1.680717e9,
//!     "f57":"600519", "f58":"贵州茅台",
//!     "f60":133069, "f86":1747724400, "f169":-710, "f170":-53,
//!     ...
//! }}
//! ```
//!
//! Unit conversions follow the README's "data source & protocol"
//! field table: price-like fields `f43/f44/f45/f46/f60/f169` are
//! integer-scaled — ÷100 for CN A-share (2 decimals), ÷1000 for
//! HK / US (3 decimals), see [`price_factor`]; `f170` (pct change)
//! is always divided by 100 (%); `f47` (volume) is multiplied by
//! 100 for CN A-share (reported in "hands") but passed through
//! unchanged for HK / US (already a share count — see
//! [`volume_factor`]); `f48` is already in yuan and passes through
//! unchanged; `f86` is a unix second count formatted as
//! `YYYY-MM-DD HH:MM:SS` in Asia/Shanghai (see `format_em_ts` for
//! why we hardcode +08:00).
//!
//! There is **no business-level retry** above the dispatch layer.
//! Transport timeouts and 5xx responses go through `HttpClient`'s
//! built-in backoff (see `http.rs`); a 404 or parse failure surfaces
//! immediately as a `SiftError`, leaving the tolerate-or-fail
//! decision to `commands/quote.rs`.

use serde_json::Value;
use time::OffsetDateTime;

use crate::domain::quote::QuoteRow;
use crate::domain::Symbol;
use crate::error::SiftError;
use crate::http::HttpClient;
use crate::sources::quote_source::QuoteSource;

use super::{em_str_to_f64, em_str_to_i64, price_factor, require_f64, secid, volume_factor};

/// Production EM quote base. Mockito tests inject a custom URL via
/// `EmQuoteUrls::for_test`; end-to-end tests can also override
/// through the `SIFT_EM_QUOTE_BASE` environment variable, mirroring
/// the `eastmoney_financials::EmUrls::from_env` pattern.
///
/// We point at the **delayed** feed (`push2delay`) rather than the
/// realtime `push2` host. The delayed feed runs ~1–3 s behind in
/// active trading hours and is functionally identical outside them;
/// EM uses it as the public retail backend. The realtime `push2`
/// hosts are routinely rate-limited or blocked at upstream CDN /
/// proxy edges when accessed from clients that look non-browser-
/// like, so picking the public retail host avoids a class of
/// "TLS handshake completes, server then RSTs the socket without
/// any HTTP response" failures that look like a sift bug but are
/// really an EM-side ACL. The trade-off is acceptable for a CLI
/// tool — sub-second realtime is not a documented sift contract.
pub const DEFAULT_QUOTE_BASE: &str = "https://push2delay.eastmoney.com";

/// Fixed `ut` token used by the public EM snapshot endpoint. Not a
/// user-level credential; the value matches `cli_design` / README.
const UT: &str = "fa5fd1943c7b386f172d6893dbfba10b";

/// Full field list. The README's field table only documents the
/// fields the first release reads (`f57=symbol`, `f43=price`, …);
/// the extras `f50/f51/f52/f62/f168/f171/f116/f117` are kept in the
/// request because the README specifies them and EM accepts the
/// larger payload at no extra cost — pre-requesting them avoids
/// changing the request side when new fields get parsed later.
const FIELDS: &str = "f43,f44,f45,f46,f47,f48,f50,f51,f52,\
                      f57,f58,f60,f62,f168,f169,f170,f171,f86,f116,f117";

#[derive(Debug, Clone)]
pub struct EmQuoteUrls {
    pub quote_base: String,
}

impl EmQuoteUrls {
    /// Production entry point — checks the env override, falls back
    /// to `DEFAULT_QUOTE_BASE`.
    pub fn from_env() -> Self {
        Self {
            quote_base: std::env::var("SIFT_EM_QUOTE_BASE")
                .unwrap_or_else(|_| DEFAULT_QUOTE_BASE.into()),
        }
    }
}

/// EM quote source — one instance per process; the trait impl in
/// turn routes `fetch` through [`quote_with_base`].
#[derive(Debug, Clone)]
pub struct EastmoneyQuoteSource {
    pub quote_base: String,
}

impl EastmoneyQuoteSource {
    pub fn from_env() -> Self {
        Self {
            quote_base: EmQuoteUrls::from_env().quote_base,
        }
    }

    #[cfg(test)]
    pub fn with_base(base: impl Into<String>) -> Self {
        Self {
            quote_base: base.into(),
        }
    }
}

/// `fetch::quote` calls this to build the registered source list in
/// `main::run_quote` — mirrors `eastmoney::bars::build`.
pub fn build() -> Box<dyn QuoteSource> {
    Box::new(EastmoneyQuoteSource::from_env())
}

impl QuoteSource for EastmoneyQuoteSource {
    fn name(&self) -> &'static str {
        "eastmoney"
    }

    fn quote(&self, symbol: &Symbol, http: &HttpClient) -> Result<QuoteRow, SiftError> {
        quote_with_base(http, symbol, &self.quote_base)
    }
}

/// Fetch a single symbol's current-price snapshot.
pub fn quote_with_base(
    http: &HttpClient,
    symbol: &Symbol,
    base: &str,
) -> Result<QuoteRow, SiftError> {
    let url = format!(
        "{base}/api/qt/stock/get?secid={secid}&fields={FIELDS}&ut={UT}",
        secid = secid(symbol),
    );
    let bytes = http.get_bytes(&url)?;
    parse(&bytes, symbol)
}

fn parse(bytes: &[u8], symbol: &Symbol) -> Result<QuoteRow, SiftError> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|e| SiftError::Parse(format!("EM stock/get not JSON: {e}")))?;
    let data = v
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| SiftError::Parse("EM stock/get: missing `data` object".into()))?;

    // Missing name = EM does not recognize this symbol (the 404
    // analog — EM does not return HTTP errors for unknown codes;
    // instead `data` is null or lacks `f57`/`f58`).
    let name = data
        .get("f58")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SiftError::NotFound(format!("{}.{}", symbol.code, symbol.market.as_upper())))?
        .to_string();

    // EM keeps the old code of a re-numbered listing (e.g. the BSE
    // 83xxxx → 92xxxx migration) alive as a placeholder whose name it
    // suffixes with "(已切换)" and whose prices are all 0. Returning
    // that 0.00 row silently would hand an agent a fake quote; treat
    // it as NotFound so it fails like `sift bars` does on the same
    // code (a `[warn]` line, no bogus data).
    if name.contains("已切换") {
        return Err(SiftError::NotFound(format!(
            "{} 已切换代码（EM: {name}）；用 `sift search` 查询新代码",
            symbol.display_symbol(),
        )));
    }

    let scale = price_factor(symbol.market);
    let price = require_f64(data, "f43")? / scale;
    let high = require_f64(data, "f44")? / scale;
    let low = require_f64(data, "f45")? / scale;
    let open = require_f64(data, "f46")? / scale;
    let prev_close = require_f64(data, "f60")? / scale;

    // `f47` is the day's volume. For CN A-share it arrives in
    // "hands" (1 hand = 100 shares) so we multiply by 100; for HK /
    // US it is already a share count and passes through unscaled.
    let volume_hand = data
        .get("f47")
        .and_then(em_str_to_f64)
        .ok_or_else(|| SiftError::Parse("missing EM f47 (volume)".into()))?;
    let volume = (volume_hand * volume_factor(symbol.market)) as i64;
    let amount = data
        .get("f48")
        .and_then(em_str_to_f64)
        .ok_or_else(|| SiftError::Parse("missing EM f48 (amount)".into()))?;

    let change = require_f64(data, "f169")? / scale;
    // `f170` is percent with a fixed ×100 encoding in every market —
    // it does not follow the price scale.
    let pct_change = require_f64(data, "f170")? / 100.0;

    let ts = data
        .get("f86")
        .and_then(em_str_to_i64)
        .ok_or_else(|| SiftError::Parse("missing EM f86 (time)".into()))?;
    let time = format_em_ts(ts);

    // The output `symbol` column uses `{code}.{UPPER_MARKET}` form
    // (consistent with `sift search`); EM's `f57` is just the bare
    // code, used here only as a sanity check.
    if let Some(em_code) = data.get("f57").and_then(Value::as_str) {
        if em_code != symbol.code {
            return Err(SiftError::Parse(format!(
                "EM returned code {em_code:?} for requested {:?}",
                symbol.code,
            )));
        }
    }
    let display_symbol = symbol.display_symbol();

    Ok(QuoteRow {
        symbol: display_symbol,
        name,
        price,
        change,
        pct_change,
        open,
        high,
        low,
        prev_close,
        volume,
        amount,
        time,
        source: "eastmoney",
    })
}

/// EM's `f86` is a unix second count (UTC). Renders as
/// `YYYY-MM-DD HH:MM:SS` in the local offset returned by
/// [`local_offset`]. If `from_unix_timestamp` rejects the value we
/// fall back to the raw seconds string so one weird timestamp can
/// never block the rest of the render pipeline.
fn format_em_ts(secs: i64) -> String {
    let Some(t) = OffsetDateTime::from_unix_timestamp(secs).ok() else {
        return secs.to_string();
    };
    let t = t.to_offset(local_offset());
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
    )
}

/// EM quote timestamps semantically belong to Asia/Shanghai — both
/// CN A-share and HK close in UTC+8. We hardcode +08:00 instead of
/// reading `/etc/localtime` for two reasons: (1) the `time` crate's
/// default feature set does not pull in `local-offset`, and enabling
/// it would drag in a tzdb dependency and lengthen the first build;
/// (2) showing the user `2026-05-20 03:00:00` because they happen to
/// run sift on a US / EU machine is more confusing than showing the
/// raw Shanghai trading time. If US quote support gets real
/// coverage later, the offset should switch based on the symbol's
/// market.
fn local_offset() -> time::UtcOffset {
    time::UtcOffset::from_hms(8, 0, 0).expect("UTC+8 is a valid offset")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::Market;

    fn http() -> HttpClient {
        HttpClient::new()
    }

    fn sym(code: &str, mkt: Market) -> Symbol {
        Symbol {
            code: code.into(),
            market: mkt,
            kind: crate::domain::market::InstrumentKind::Equity,
        }
    }

    fn em_body(name: &str) -> String {
        // All values use EM's raw scale: prices ×100, change ×100,
        // pct_change ×100, volume = hands (÷100 of the share count
        // sift emits).
        format!(
            r#"{{
                "rc":0, "rt":4, "data": {{
                    "f43":132359, "f44":133299, "f45":132000, "f46":132100,
                    "f47":12674, "f48":1680717318.0,
                    "f50":0, "f51":0, "f52":0, "f62":0,
                    "f57":"600519", "f58":"{name}", "f60":133069,
                    "f86":1747724400,
                    "f168":0, "f169":-710, "f170":-53, "f171":0,
                    "f116":0, "f117":0
                }}
            }}"#,
        )
    }

    #[test]
    fn parses_and_normalizes_units() {
        let body = em_body("贵州茅台");
        let row = parse(body.as_bytes(), &sym("600519", Market::CnA)).unwrap();
        assert_eq!(row.symbol, "600519.CN-A");
        assert_eq!(row.name, "贵州茅台");
        assert!((row.price - 1323.59).abs() < 1e-6);
        assert!((row.high - 1332.99).abs() < 1e-6);
        assert!((row.low - 1320.00).abs() < 1e-6);
        assert!((row.open - 1321.00).abs() < 1e-6);
        assert!((row.prev_close - 1330.69).abs() < 1e-6);
        // hands → shares: 12674 × 100 = 1_267_400
        assert_eq!(row.volume, 1_267_400);
        assert!((row.amount - 1_680_717_318.0).abs() < 1e-3);
        assert!((row.change - -7.10).abs() < 1e-6);
        assert!((row.pct_change - -0.53).abs() < 1e-6);
        // ISO format `YYYY-MM-DD HH:MM:SS` is at least 19 chars.
        assert!(row.time.len() >= 19, "time was {:?}", row.time);
        assert!(row.time.contains('-') && row.time.contains(':'));
        assert_eq!(row.source, "eastmoney");
    }

    #[test]
    fn hk_volume_is_already_shares_not_hands() {
        // Same wire body, but parsed as an HK symbol: `f47` must pass
        // through unscaled (12674 shares, not 12674×100), and the
        // price fields use the HK 3-decimal scale (132359 → 132.359,
        // not 1323.59).
        let body = em_body("腾讯控股").replace("\"f57\":\"600519\"", "\"f57\":\"00700\"");
        let row = parse(body.as_bytes(), &sym("00700", Market::Hk)).unwrap();
        assert_eq!(row.symbol, "00700.HK");
        assert_eq!(row.volume, 12_674);
        assert!((row.price - 132.359).abs() < 1e-6);
        assert!((row.change - -0.71).abs() < 1e-6); // f169 follows the price scale (÷1000 for HK)
        assert!((row.pct_change - -0.53).abs() < 1e-6); // f170 is always ÷100
    }

    #[test]
    fn switched_code_placeholder_is_not_found_not_a_zero_quote() {
        // EM keeps a re-numbered listing's old code alive with a
        // "(已切换)" name and all-zero prices (the BSE 83xxxx→92xxxx
        // migration). That must surface as NotFound, not a 0.00 row.
        let body = em_body("梓橦宫(已切换)").replace("\"f57\":\"600519\"", "\"f57\":\"832566\"");
        let err = parse(body.as_bytes(), &sym("832566", Market::CnA)).unwrap_err();
        assert!(matches!(err, SiftError::NotFound(_)), "got {err:?}");
        assert!(err.to_string().contains("已切换"), "msg: {err}");
    }

    #[test]
    fn missing_data_object_yields_parse_error() {
        let body = r#"{"rc":0,"data":null}"#;
        let err = parse(body.as_bytes(), &sym("600519", Market::CnA)).unwrap_err();
        assert!(matches!(err, SiftError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn missing_name_treated_as_not_found() {
        // EM's typical "no such symbol" response: `data` is present
        // but `f58` is an empty string.
        let body = r#"{"rc":0,"data":{"f58":""}}"#;
        let err = parse(body.as_bytes(), &sym("999999", Market::CnA)).unwrap_err();
        assert!(matches!(err, SiftError::NotFound(_)), "got {err:?}");
    }

    #[test]
    fn code_mismatch_yields_parse_error() {
        let body = em_body("贵州茅台").replace("\"f57\":\"600519\"", "\"f57\":\"600520\"");
        let err = parse(body.as_bytes(), &sym("600519", Market::CnA)).unwrap_err();
        assert!(matches!(err, SiftError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn source_trait_routes_to_quote_with_base() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/api/qt/stock/get")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(em_body("贵州茅台"))
            .expect_at_least(1)
            .create();
        let src = EastmoneyQuoteSource::with_base(server.url());
        assert_eq!(src.name(), "eastmoney");
        let row = src
            .quote(&sym("600519", Market::CnA), &HttpClient::new())
            .unwrap();
        assert_eq!(row.name, "贵州茅台");
        m.assert();
    }

    #[test]
    fn quote_with_base_round_trips_through_mockito() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/api/qt/stock/get")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(em_body("贵州茅台"))
            .expect_at_least(1)
            .create();

        let row = quote_with_base(&http(), &sym("600519", Market::CnA), &server.url()).unwrap();
        assert_eq!(row.name, "贵州茅台");
        m.assert();
    }

    #[test]
    fn quote_with_base_propagates_404() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/api/qt/stock/get")
            .match_query(mockito::Matcher::Any)
            .with_status(404)
            .with_body("not found")
            .create();

        let err = quote_with_base(&http(), &sym("999999", Market::CnA), &server.url())
            .unwrap_err();
        // HttpClient wraps non-2xx into Network and does not retry
        // 404; the error surfaces immediately.
        assert!(matches!(err, SiftError::Network(_)), "got {err:?}");
    }

    #[test]
    fn quote_with_base_retries_5xx_then_succeeds() {
        let mut server = mockito::Server::new();
        // Two 502s followed by a 200. BACKOFF_SECS is all zeros
        // under cfg(test).
        let _bad = server
            .mock("GET", "/api/qt/stock/get")
            .match_query(mockito::Matcher::Any)
            .with_status(502)
            .with_body("upstream down")
            .expect(2)
            .create();
        let _ok = server
            .mock("GET", "/api/qt/stock/get")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(em_body("贵州茅台"))
            .expect(1)
            .create();

        let row = quote_with_base(&http(), &sym("600519", Market::CnA), &server.url()).unwrap();
        assert_eq!(row.name, "贵州茅台");
    }
}
