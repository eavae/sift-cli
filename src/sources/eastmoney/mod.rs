//! East Money **market-data** integration (F5). Distinct from its
//! sibling `sources/eastmoney_financials/` — this subdirectory only
//! covers current-price snapshots and historical daily K-lines, with
//! its own endpoint set, field semantics, and unit conversions.
//!
//! - `quote` — single-shot snapshot via
//!   `push2delay.eastmoney.com/api/qt/stock/get` (EM's public retail
//!   feed; see `quote::DEFAULT_QUOTE_BASE` for why we avoid the
//!   realtime `push2` host)
//! - `bars`  — daily K-line via
//!   `push2his.eastmoney.com/api/qt/stock/kline/get` (story-02)
//!
//! Helpers shared at the module root:
//! - `secid()`: [`crate::domain::Symbol`] → EM `secid` string
//!   (`1.600519` / `0.000001` / `116.00700` / `105.AAPL`).
//! - `EmQuoteUrls` / `EmBarsUrls`: two independent base URLs (EM's
//!   snapshot and K-line endpoints live on different hosts, so a
//!   single base won't do). Each carries a `from_env` that reads
//!   `SIFT_EM_QUOTE_BASE` / `SIFT_EM_BARS_BASE` so mockito tests can
//!   redirect the request without touching the binary.

pub mod bars;
pub mod quote;

pub use bars::{bars_daily_with_base, BarsQuery, EmBarsUrls};
pub use quote::{quote_with_base, EmQuoteUrls};

use crate::domain::market::Market;
use crate::domain::Symbol;
use crate::error::SiftError;

/// Translate a [`Symbol`] into EM's `secid` string.
///
/// CN A-share splits Shanghai vs. Shenzhen/Beijing by code prefix:
/// - `1` = Shanghai (`600` / `601` / `603` / `605` / `688` / `689` /
///   `900`)
/// - `0` = Shenzhen / Beijing (everything else under `Market::CnA`)
///
/// HK is always `116`; US is always `105` (NASDAQ — the first sift
/// release does not split NYSE / AMEX; see the US note in
/// [`docs/f5-realtime/README.md`]). The US branch is currently dead
/// code because `Symbol::parse` falls into `unreachable!()` for
/// `Market::Us`, but the mapping is kept here so the table reads
/// completely.
pub(crate) fn secid(sym: &Symbol) -> String {
    let prefix = match sym.market {
        Market::Hk => "116",
        Market::Us => "105",
        Market::CnA => {
            if is_shanghai_code(&sym.code) {
                "1"
            } else {
                "0"
            }
        }
    };
    format!("{prefix}.{}", sym.code)
}

/// CN-A only: is this code in the Shanghai range? Aligned with
/// [`crate::domain::market::infer_board`]'s mapping but collapsed to
/// the binary "Shanghai vs. not-Shanghai" distinction we need here.
fn is_shanghai_code(code: &str) -> bool {
    matches!(
        code.get(..3),
        Some("600") | Some("601") | Some("603") | Some("605") | Some("688") | Some("689") | Some("900")
    )
}

/// Safely coerce one of EM's stringified numbers (e.g. `"-"`, empty
/// string) into an `f64`. Upstream uses `"-"` to mean "no data";
/// we map it to `None`. If a caller ever needs to distinguish
/// "actual zero" from "no data", lift the return type to
/// `Option<f64>` end-to-end.
pub(crate) fn em_str_to_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() || s == "-" {
                None
            } else {
                s.parse::<f64>().ok()
            }
        }
        _ => None,
    }
}

/// Same as [`em_str_to_f64`] but for integer fields — EM's `f86`
/// timestamp is a stringified unix second count.
pub(crate) fn em_str_to_i64(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Required EM field accessor: a missing/non-numeric value means
/// upstream returned an unexpected shape → `SiftError::Parse`.
pub(crate) fn require_f64(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<f64, SiftError> {
    obj.get(key)
        .and_then(em_str_to_f64)
        .ok_or_else(|| SiftError::Parse(format!("missing or non-numeric EM field {key:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(code: &str, mkt: Market) -> Symbol {
        Symbol {
            code: code.into(),
            market: mkt,
        }
    }

    #[test]
    fn secid_uses_1_for_shanghai_and_0_for_shenzhen_and_beijing() {
        assert_eq!(secid(&sym("600519", Market::CnA)), "1.600519");
        assert_eq!(secid(&sym("688123", Market::CnA)), "1.688123");
        assert_eq!(secid(&sym("900901", Market::CnA)), "1.900901"); // SH B-share
        assert_eq!(secid(&sym("000001", Market::CnA)), "0.000001");
        assert_eq!(secid(&sym("002415", Market::CnA)), "0.002415");
        assert_eq!(secid(&sym("300750", Market::CnA)), "0.300750");
        assert_eq!(secid(&sym("200011", Market::CnA)), "0.200011"); // SZ B-share
        assert_eq!(secid(&sym("832000", Market::CnA)), "0.832000"); // Beijing
    }

    #[test]
    fn secid_for_hk_is_116() {
        assert_eq!(secid(&sym("00700", Market::Hk)), "116.00700");
        assert_eq!(secid(&sym("00388", Market::Hk)), "116.00388");
    }

    #[test]
    fn em_str_to_f64_handles_string_number_and_dash() {
        assert_eq!(em_str_to_f64(&serde_json::json!("12.34")), Some(12.34));
        assert_eq!(em_str_to_f64(&serde_json::json!(12.34)), Some(12.34));
        assert_eq!(em_str_to_f64(&serde_json::json!("-")), None);
        assert_eq!(em_str_to_f64(&serde_json::json!("")), None);
        assert_eq!(em_str_to_f64(&serde_json::json!(null)), None);
    }
}
