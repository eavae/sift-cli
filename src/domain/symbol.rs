//! `Symbol` parser. Accepts the three forms documented in story 02 §3:
//! `600519` (bare digits) / `sh600519` / `600519.SH` (and `.HK`, etc.).
//! Case-insensitive on the market suffix; leading zeros are preserved.
//!
//! Explicit `sh` / `sz` prefixes also unlock **index** symbols: the
//! exchange-specific index segments are `000xxx` (SH — 上证指数
//! `sh000001`) and `399xxx` (SZ — 深证成指 `sz399001`). A prefix that
//! contradicts the code's own exchange segment (`sh399001`,
//! `sz600519`) is a parse error — fail fast beats silently querying
//! the wrong instrument.

use super::market::{InstrumentKind, Market};
use crate::error::SiftError;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Symbol {
    /// Numeric code with leading zeros preserved: 6 digits for CN A-share,
    /// 5 digits for HK. Always lowercase ASCII digits (i.e. just digits).
    pub code: String,
    pub market: Market,
    /// Equity vs. index. Indices are accepted by `quote` / `bars` only;
    /// the fundamentals commands reject them before dispatch.
    pub kind: InstrumentKind,
}

impl Symbol {
    /// Parse one of the supported literal forms into a `Symbol`. We do
    /// not retain the raw user string — downstream code reformats from
    /// `(code, market)` whenever a display form is needed.
    ///
    /// An explicit suffix (`sh` / `sz` / `bj` / `hk`) is taken at face
    /// value as long as it does not contradict the code's exchange
    /// segment (`sh` + `000xxx` is the SH index, not a stock —
    /// that combination resolves to `InstrumentKind::Index`).
    pub fn parse(input: &str) -> Result<Self, SiftError> {
        let s = input.trim().to_ascii_lowercase();
        if s.is_empty() {
            return Err(SiftError::Parse("empty symbol".into()));
        }

        // 1) `600519.sh` / `00700.hk`
        if let Some((code, mkt)) = s.split_once('.') {
            return assemble(code, Some(mkt));
        }

        // 2) `sh600519` / `sz000001` / `bj430718` / `hk00700`
        for prefix in ["sh", "sz", "bj", "hk"] {
            if let Some(rest) = s.strip_prefix(prefix) {
                return assemble(rest, Some(prefix));
            }
        }

        // 3) Bare digits — infer market from length.
        assemble(&s, None)
    }

    /// Display form used in output rows. Equities use the
    /// `{code}.{MARKET}` form (`600519.CN-A`); indices use the
    /// exchange-prefixed bare form (`sh000001`) so an index can never
    /// be confused with the same-code stock of the other exchange
    /// (`000001.CN-A` is always 平安银行).
    pub fn display_symbol(&self) -> String {
        if self.kind == InstrumentKind::Index {
            let prefix = if self.code.starts_with("000") {
                "sh"
            } else {
                "sz"
            };
            return format!("{prefix}{}", self.code);
        }
        format!("{}.{}", self.code, self.market.as_upper())
    }
}

fn assemble(code: &str, mkt_hint: Option<&str>) -> Result<Symbol, SiftError> {
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err(SiftError::Parse(format!(
            "expected digits, got {code:?}"
        )));
    }
    let (market, kind) = match mkt_hint {
        Some("sh") => sh_or_sz(code, "sh")?,
        Some("sz") => sh_or_sz(code, "sz")?,
        Some("bj") => (Market::CnA, InstrumentKind::Equity),
        Some("hk") => (Market::Hk, InstrumentKind::Equity),
        Some(other) => {
            return Err(SiftError::Parse(format!(
                "unknown market suffix {other:?}"
            )));
        }
        // Bare digits keep the historical behavior: 6-digit codes are
        // always equities (`000001` is 平安银行, never the SH index —
        // indices require the explicit prefix), 5-digit is HK.
        None => match code.len() {
            6 => (Market::CnA, InstrumentKind::Equity),
            5 => (Market::Hk, InstrumentKind::Equity),
            n => {
                return Err(SiftError::Parse(format!(
                    "expected 5 or 6 digits, got {n}-digit {code:?}"
                )));
            }
        },
    };
    let expected_len = match market {
        Market::CnA => 6,
        Market::Hk => 5,
        // `Us` is not reachable here: `assemble` only resolves to `CnA`
        // (sh/sz/bj or 6-digit) or `Hk` (hk or 5-digit). US support via
        // Symbol::parse would require its own suffix branch.
        Market::Us => unreachable!("Symbol::parse does not yield Market::Us"),
    };
    if code.len() != expected_len {
        return Err(SiftError::Parse(format!(
            "expected {expected_len}-digit code for {market:?}, got {code:?}"
        )));
    }
    Ok(Symbol {
        code: code.to_string(),
        market,
        kind,
    })
}

/// Resolve an explicit `sh` / `sz` prefix against the code's segment.
///
/// - `sh` + `000xxx` → SH index; `sz` + `399xxx` → SZ index.
/// - A prefix contradicting a segment that exists **only** on the
///   other exchange (`sz600519`, `sh399001`) is rejected.
/// - Unknown segments are accepted at face value (forward-compatible
///   with new boards), matching the pre-index behavior.
fn sh_or_sz(code: &str, prefix: &str) -> Result<(Market, InstrumentKind), SiftError> {
    let kind = match prefix {
        "sh" if code.starts_with("000") => InstrumentKind::Index,
        "sz" if code.starts_with("399") => InstrumentKind::Index,
        "sh" if is_sz_only_segment(code) => {
            return Err(SiftError::Parse(format!(
                "{prefix}{code}: code segment belongs to SZ, not SH"
            )));
        }
        "sz" if is_sh_only_segment(code) => {
            return Err(SiftError::Parse(format!(
                "{prefix}{code}: code segment belongs to SH, not SZ"
            )));
        }
        _ => InstrumentKind::Equity,
    };
    Ok((Market::CnA, kind))
}

/// Segments that only exist on the Shanghai exchange (stocks and
/// SH B-shares). `000xxx` is deliberately absent: SH uses it for
/// indexes while SZ uses it for stocks — the prefix disambiguates.
fn is_sh_only_segment(code: &str) -> bool {
    matches!(
        code.get(..3),
        Some("600") | Some("601") | Some("603") | Some("605") | Some("688") | Some("689") | Some("900")
    )
}

/// Segments that only exist on the Shenzhen exchange (SZ stocks,
/// SZ B-shares, and the `399xxx` index segment).
fn is_sz_only_segment(code: &str) -> bool {
    matches!(
        code.get(..3),
        Some("001") | Some("002") | Some("003") | Some("300") | Some("301") | Some("200") | Some("399")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::{infer_board, Board};

    #[test]
    fn bare_six_digit_is_cn_a_with_board() {
        let s = Symbol::parse("600519").unwrap();
        assert_eq!(s.code, "600519");
        assert_eq!(s.market, Market::CnA);
        assert_eq!(infer_board(&s.code), Some(Board::ShMain));
    }

    #[test]
    fn bare_five_digit_is_hk_with_leading_zero_preserved() {
        let s = Symbol::parse("00700").unwrap();
        assert_eq!(s.code, "00700");
        assert_eq!(s.market, Market::Hk);
    }

    #[test]
    fn dot_suffix_and_prefix_forms_are_equivalent() {
        let a = Symbol::parse("600519.SH").unwrap();
        let b = Symbol::parse("sh600519").unwrap();
        let c = Symbol::parse("SH600519").unwrap();
        let d = Symbol::parse("  600519.sh  ").unwrap(); // trimmed
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(c, d);
        assert_eq!(a.code, "600519");
        assert_eq!(a.market, Market::CnA);
    }

    #[test]
    fn hk_dot_suffix() {
        let s = Symbol::parse("00700.HK").unwrap();
        assert_eq!(s.code, "00700");
        assert_eq!(s.market, Market::Hk);
    }

    #[test]
    fn explicit_sh_prefix_on_000_segment_is_the_sh_index() {
        // `sh000001` — 上证指数; the SH 000xxx segment has no stocks,
        // so this combination resolves to an index.
        let s = Symbol::parse("sh000001").unwrap();
        assert_eq!(s.code, "000001");
        assert_eq!(s.market, Market::CnA);
        assert_eq!(s.kind, InstrumentKind::Index);
        assert_eq!(s.display_symbol(), "sh000001");
    }

    #[test]
    fn explicit_sz_prefix_on_399_segment_is_the_sz_index() {
        let s = Symbol::parse("sz399001").unwrap();
        assert_eq!(s.kind, InstrumentKind::Index);
        assert_eq!(s.display_symbol(), "sz399001");
    }

    #[test]
    fn explicit_sz_prefix_on_000_segment_stays_equity() {
        // `sz000001` is 平安银行 — the 000xxx segment is a stock on SZ.
        let s = Symbol::parse("sz000001").unwrap();
        assert_eq!(s.market, Market::CnA);
        assert_eq!(s.kind, InstrumentKind::Equity);
        assert_eq!(s.display_symbol(), "000001.CN-A");
    }

    #[test]
    fn bare_000001_stays_equity_for_backward_compatibility() {
        let s = Symbol::parse("000001").unwrap();
        assert_eq!(s.kind, InstrumentKind::Equity);
    }

    #[test]
    fn contradicting_prefix_and_segment_is_parse_error() {
        assert!(matches!(Symbol::parse("sz600519"), Err(SiftError::Parse(_))));
        assert!(matches!(Symbol::parse("sh399001"), Err(SiftError::Parse(_))));
        assert!(matches!(Symbol::parse("sz688123"), Err(SiftError::Parse(_))));
        assert!(matches!(Symbol::parse("sh300750"), Err(SiftError::Parse(_))));
    }

    #[test]
    fn non_digit_input_is_parse_error() {
        assert!(matches!(Symbol::parse("abc"), Err(SiftError::Parse(_))));
        assert!(matches!(Symbol::parse("60051a"), Err(SiftError::Parse(_))));
    }

    #[test]
    fn wrong_length_is_parse_error() {
        // 4 digits — neither HK nor A-share.
        assert!(matches!(Symbol::parse("1234"), Err(SiftError::Parse(_))));
        // 7 digits.
        assert!(matches!(Symbol::parse("1234567"), Err(SiftError::Parse(_))));
        // Empty.
        assert!(matches!(Symbol::parse("   "), Err(SiftError::Parse(_))));
    }

    #[test]
    fn unknown_suffix_is_parse_error() {
        assert!(matches!(
            Symbol::parse("600519.US"),
            Err(SiftError::Parse(_))
        ));
    }

    #[test]
    fn case_insensitive_suffix() {
        assert_eq!(
            Symbol::parse("00700.hk").unwrap(),
            Symbol::parse("00700.HK").unwrap()
        );
    }
}
