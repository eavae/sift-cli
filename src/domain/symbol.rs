//! `Symbol` parser. Accepts the three forms documented in story 02 §3:
//! `600519` (bare digits) / `sh600519` / `600519.SH` (and `.HK`, etc.).
//! Case-insensitive on the market suffix; leading zeros are preserved.

use super::market::Market;
use crate::error::SiftError;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Symbol {
    /// Numeric code with leading zeros preserved: 6 digits for CN A-share,
    /// 5 digits for HK. Always lowercase ASCII digits (i.e. just digits).
    pub code: String,
    pub market: Market,
}

impl Symbol {
    /// Parse one of the supported literal forms into a `Symbol`. We do
    /// not retain the raw user string — downstream code reformats from
    /// `(code, market)` whenever a display form is needed.
    ///
    /// An explicit suffix (`sh` / `sz` / `bj` / `hk`) is taken at face
    /// value even when it disagrees with what the prefix would suggest
    /// (e.g. `sh000001`). User input is truth; we do not "auto-correct"
    /// here — that is a product decision.
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
}

fn assemble(code: &str, mkt_hint: Option<&str>) -> Result<Symbol, SiftError> {
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err(SiftError::Parse(format!(
            "expected digits, got {code:?}"
        )));
    }
    let market = match mkt_hint {
        Some("sh") | Some("sz") | Some("bj") => Market::CnA,
        Some("hk") => Market::Hk,
        Some(other) => {
            return Err(SiftError::Parse(format!(
                "unknown market suffix {other:?}"
            )));
        }
        None => match code.len() {
            6 => Market::CnA,
            5 => Market::Hk,
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
    })
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
    fn explicit_suffix_overrides_prefix_inference() {
        // `sh000001` — user said sh, so we trust them (cninfo SH index
        // historically used this code).
        let s = Symbol::parse("sh000001").unwrap();
        assert_eq!(s.code, "000001");
        assert_eq!(s.market, Market::CnA);
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
