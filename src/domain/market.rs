//! Market / Board enums plus the local `code` → board dictionary.

/// First-version market scope: CN A-share + HK. `Us` / `Neeq` are
/// explicitly out.
///
/// Declaration order is significant: `CnA` is declared first, so the
/// derived `Ord` impl gives "CN-A before HK", matching the README sort
/// convention used by every command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Market {
    /// CN A-share (sh + sz + bj + B-share + CDR), keyed on the `cninfo`
    /// `szse_stock.json` endpoint despite its misleading name.
    CnA,
    /// Hong Kong main board / GEM, served by `hke_stock.json`.
    Hk,
    /// US-listed equities. Reached only by the eastmoney financial
    /// source; `sift search` does not enumerate US tickers (cninfo does
    /// not cover them).
    Us,
}

impl Market {
    /// Machine-readable lowercase label (used in `--format json`).
    pub fn as_lower(self) -> &'static str {
        match self {
            Market::CnA => "cn-a",
            Market::Hk => "hk",
            Market::Us => "us",
        }
    }

    /// Human-readable uppercase label (used in the default table renderer).
    pub fn as_upper(self) -> &'static str {
        match self {
            Market::CnA => "CN-A",
            Market::Hk => "HK",
            Market::Us => "US",
        }
    }
}

/// CN A-share sub-boards. HK is intentionally a single bucket — cninfo
/// does not separate Main / GEM and we do not want to invent the split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Board {
    ShMain,
    ShStar,
    SzMain,
    SzSme,
    SzGem,
    BShare,
    BjMain,
}

impl Board {
    /// Lowercase kebab-case label used in both NDJSON and table output.
    pub fn as_str(self) -> &'static str {
        match self {
            Board::ShMain => "sh-main",
            Board::ShStar => "sh-star",
            Board::SzMain => "sz-main",
            Board::SzSme => "sz-sme",
            Board::SzGem => "sz-gem",
            Board::BShare => "b-share",
            Board::BjMain => "bj-main",
        }
    }
}

/// Infer the sub-board from a raw `code`. Returns `None` for unknown
/// prefixes; the caller decides how to render that (e.g. blank cell).
/// HK codes (5 digits) always return `None` — they do not have a
/// sub-board in this dictionary.
pub fn infer_board(code: &str) -> Option<Board> {
    let p3 = code.get(..3)?;
    Some(match p3 {
        "600" | "601" | "603" | "605" => Board::ShMain,
        "688" | "689" => Board::ShStar,
        "000" | "001" | "003" => Board::SzMain,
        "002" => Board::SzSme,
        "300" | "301" => Board::SzGem,
        "200" | "900" => Board::BShare,
        "430" | "920" => Board::BjMain,
        _ if code.len() == 6 && code.starts_with('8') => Board::BjMain,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_label_pairs() {
        assert_eq!(Market::CnA.as_lower(), "cn-a");
        assert_eq!(Market::CnA.as_upper(), "CN-A");
        assert_eq!(Market::Hk.as_lower(), "hk");
        assert_eq!(Market::Hk.as_upper(), "HK");
        assert_eq!(Market::Us.as_lower(), "us");
        assert_eq!(Market::Us.as_upper(), "US");
    }

    #[test]
    fn market_orders_cn_a_before_hk_before_us() {
        assert!(Market::CnA < Market::Hk);
        assert!(Market::Hk < Market::Us);
        let mut v = vec![Market::Us, Market::Hk, Market::CnA];
        v.sort();
        assert_eq!(v, vec![Market::CnA, Market::Hk, Market::Us]);
    }

    #[test]
    fn board_label_table() {
        assert_eq!(Board::ShMain.as_str(), "sh-main");
        assert_eq!(Board::ShStar.as_str(), "sh-star");
        assert_eq!(Board::SzMain.as_str(), "sz-main");
        assert_eq!(Board::SzSme.as_str(), "sz-sme");
        assert_eq!(Board::SzGem.as_str(), "sz-gem");
        assert_eq!(Board::BShare.as_str(), "b-share");
        assert_eq!(Board::BjMain.as_str(), "bj-main");
    }

    #[test]
    fn infer_board_covers_each_readme_row() {
        // One representative `code` per README row.
        assert_eq!(infer_board("600519"), Some(Board::ShMain));
        assert_eq!(infer_board("601398"), Some(Board::ShMain));
        assert_eq!(infer_board("603259"), Some(Board::ShMain));
        assert_eq!(infer_board("605358"), Some(Board::ShMain));

        assert_eq!(infer_board("688123"), Some(Board::ShStar));
        assert_eq!(infer_board("689009"), Some(Board::ShStar));

        assert_eq!(infer_board("000001"), Some(Board::SzMain));
        assert_eq!(infer_board("001202"), Some(Board::SzMain));
        assert_eq!(infer_board("003816"), Some(Board::SzMain));

        assert_eq!(infer_board("002415"), Some(Board::SzSme));

        assert_eq!(infer_board("300750"), Some(Board::SzGem));
        assert_eq!(infer_board("301029"), Some(Board::SzGem));

        assert_eq!(infer_board("200012"), Some(Board::BShare));
        assert_eq!(infer_board("900901"), Some(Board::BShare));

        assert_eq!(infer_board("430510"), Some(Board::BjMain));
        assert_eq!(infer_board("920002"), Some(Board::BjMain));
        // 8xx 6-digit (e.g. 830799 / 870299 / 832000)
        assert_eq!(infer_board("830799"), Some(Board::BjMain));
        assert_eq!(infer_board("870299"), Some(Board::BjMain));
    }

    #[test]
    fn infer_board_unknown_prefix_is_none() {
        assert_eq!(infer_board("555555"), None);
        assert_eq!(infer_board("999999"), None);
        // Too short to even slice the 3-char prefix.
        assert_eq!(infer_board("12"), None);
    }

}
