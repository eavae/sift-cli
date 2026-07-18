//! Tencent finance (`ifzq.gtimg.cn`) source — historical K-line
//! integration (default source for `sift bars`).
//!
//! The single endpoint covers CN A-share, HK and US under three
//! sibling paths (`fqkline` / `hkfqkline` / `usfqkline`), keyed by
//! a per-market symbol prefix (`sh` / `sz` / `bj` / `hk` / `us.`).
//! Daily / weekly / monthly K is served natively; quarterly /
//! yearly is intentionally not supported (Tencent does not return
//! data for those `period` values, and the unified bars schema is
//! deliberately scoped to the three shared periods).
//!
//! Helpers shared at the module root:
//! - `tencent_code()`: [`crate::domain::Symbol`] → Tencent symbol
//!   key (`sh600519` / `hk00700` / `us.AAPL`). Mirrors the
//!   `secid()` helper in `eastmoney`; kept separate because the
//!   formats are different conventions.
//! - `kline_path()`: market → API path (`fqkline` for A-share,
//!   `hkfqkline` for HK, `usfqkline` for US).

pub mod bars;

// Concrete source type lives in `bars`; production uses
// `bars::build()` to get a trait object. No re-exports at this
// level — keeps the surface minimal.

use crate::domain::market::{InstrumentKind, Market};
use crate::domain::Symbol;

/// Translate a [`Symbol`] into Tencent's symbol key. CN A-share
/// splits SH / SZ / BJ by code prefix:
/// - SH: codes starting with 600/601/603/605/688/689/900
/// - BJ: codes starting with 4 (43x), 8 (8xx) or 920
/// - SZ: everything else
///
/// CN indexes map to the same `sh` / `sz` prefixes (`sh000001` =
/// 上证指数, `sz399001` = 深证成指) — Tencent serves index K-lines
/// from the same `fqkline` endpoint.
///
/// Beijing (BJ) is exposed because Tencent does serve 北交所 quotes
/// via `bj430000` style codes — keeping that future-proofed even
/// though the rest of sift's CN-A coverage does not currently
/// distinguish BJ as its own dispatch dimension.
pub(crate) fn tencent_code(sym: &Symbol) -> String {
    match sym.market {
        Market::Hk => format!("hk{}", sym.code),
        Market::Us => format!("us.{}", sym.code),
        Market::CnA => {
            if sym.kind == InstrumentKind::Index {
                // SH indexes occupy 000xxx, SZ indexes 399xxx.
                let prefix = if sym.code.starts_with("000") { "sh" } else { "sz" };
                return format!("{prefix}{}", sym.code);
            }
            let prefix = if is_shanghai_code(&sym.code) {
                "sh"
            } else if is_beijing_code(&sym.code) {
                "bj"
            } else {
                "sz"
            };
            format!("{prefix}{}", sym.code)
        }
    }
}

/// Tencent path segment per market. The three paths share the same
/// query-string contract but differ in column count of the kline
/// row (A-share: 6 cols, HK: 9 cols with a dividend metadata
/// object at index 6, US: 11 cols with extra trailing metrics).
/// The parser handles all three by only consuming the first six
/// positional fields and ignoring any trailing payload.
pub(crate) fn kline_path(sym: &Symbol) -> &'static str {
    match sym.market {
        Market::Hk => "hkfqkline",
        Market::Us => "usfqkline",
        Market::CnA => "fqkline",
    }
}

fn is_shanghai_code(code: &str) -> bool {
    matches!(
        code.get(..3),
        Some("600") | Some("601") | Some("603") | Some("605") | Some("688") | Some("689") | Some("900")
    )
}

fn is_beijing_code(code: &str) -> bool {
    // 43xxxx / 83xxxx / 87xxxx / 88xxxx / 92xxxx — the BJ exchange
    // ranges. cninfo's listing dictionary uses the same ranges; we
    // duplicate the check here to keep `sources/tencent` self-
    // contained.
    let p1 = code.chars().next();
    let p2 = code.get(..2);
    matches!(p2, Some("43") | Some("83") | Some("87") | Some("88") | Some("92"))
        || matches!(p1, Some('4') | Some('8') | Some('9')) && code.len() == 6 && !is_shanghai_code(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(code: &str, mkt: Market) -> Symbol {
        Symbol {
            code: code.into(),
            market: mkt,
            kind: crate::domain::market::InstrumentKind::Equity,
        }
    }

    #[test]
    fn cn_a_codes_get_sh_or_sz_prefix() {
        assert_eq!(tencent_code(&sym("600519", Market::CnA)), "sh600519");
        assert_eq!(tencent_code(&sym("688123", Market::CnA)), "sh688123");
        assert_eq!(tencent_code(&sym("900901", Market::CnA)), "sh900901");
        assert_eq!(tencent_code(&sym("000001", Market::CnA)), "sz000001");
        assert_eq!(tencent_code(&sym("002415", Market::CnA)), "sz002415");
        assert_eq!(tencent_code(&sym("300750", Market::CnA)), "sz300750");
        assert_eq!(tencent_code(&sym("200011", Market::CnA)), "sz200011"); // SZ B-share
    }

    #[test]
    fn beijing_codes_get_bj_prefix() {
        assert_eq!(tencent_code(&sym("832000", Market::CnA)), "bj832000");
        assert_eq!(tencent_code(&sym("430510", Market::CnA)), "bj430510");
        assert_eq!(tencent_code(&sym("870299", Market::CnA)), "bj870299");
        assert_eq!(tencent_code(&sym("920002", Market::CnA)), "bj920002");
    }

    #[test]
    fn cn_index_codes_get_sh_or_sz_prefix() {
        let sh_idx = Symbol {
            code: "000001".into(),
            market: Market::CnA,
            kind: InstrumentKind::Index,
        };
        let sz_idx = Symbol {
            code: "399001".into(),
            market: Market::CnA,
            kind: InstrumentKind::Index,
        };
        assert_eq!(tencent_code(&sh_idx), "sh000001"); // 上证指数
        assert_eq!(tencent_code(&sz_idx), "sz399001"); // 深证成指
    }

    #[test]
    fn hk_codes_get_hk_prefix() {
        assert_eq!(tencent_code(&sym("00700", Market::Hk)), "hk00700");
        assert_eq!(tencent_code(&sym("00388", Market::Hk)), "hk00388");
    }

    #[test]
    fn us_codes_get_us_dot_prefix() {
        // The `us.` form is Tencent's convention; the dot
        // distinguishes US tickers from numeric CN codes.
        assert_eq!(tencent_code(&sym("AAPL", Market::Us)), "us.AAPL");
    }

    #[test]
    fn path_per_market() {
        assert_eq!(kline_path(&sym("600519", Market::CnA)), "fqkline");
        assert_eq!(kline_path(&sym("00700", Market::Hk)), "hkfqkline");
        assert_eq!(kline_path(&sym("AAPL", Market::Us)), "usfqkline");
    }
}
