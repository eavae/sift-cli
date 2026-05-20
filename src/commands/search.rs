//! `sift search`: fuzzy lookup over the cached cninfo A-share + HK
//! listings. Three-way match (`code` substring / `zwjc` Chinese
//! substring / `pinyin` initials prefix) per the F1 README; output
//! goes through the format dispatcher in [`crate::output`].

use serde::{Serialize, Serializer};

use crate::cache::search::{fetch_stock_lists, SearchCacheOpts};
use crate::cli::SearchArgs;
use crate::domain::market::{infer_board, Board, Market};
use crate::error::SiftError;
use crate::http::HttpClient;
use crate::output::{self, Format, RenderRow};
use crate::sources::cninfo::{CnInfoRow, StockLists};

/// One row of the search result. Field *order* is what `serde_json`
/// emits for `--format json`; the table / tsv column order is defined
/// independently by [`RenderRow::headers`].
///
/// `market` and `board` hold the typed enum values; the lowercase /
/// uppercase / dash literal mappings live on the enums themselves (so
/// F2 / F3 / F5 can reuse them) and are applied via `serialize_with`
/// for JSON and via [`RenderRow::cells`] for table / tsv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchHit {
    pub code: String,
    #[serde(rename = "zwjc")]
    pub name: String,
    pub pinyin: String,
    pub category: String,
    #[serde(rename = "orgId")]
    pub org_id: String,
    #[serde(serialize_with = "serialize_market_lower")]
    pub market: Market,
    #[serde(serialize_with = "serialize_board_lower")]
    pub board: Option<Board>,
    /// Always `"cninfo"` in F1; reserved for future multi-source work.
    pub source: &'static str,
}

fn serialize_market_lower<S: Serializer>(m: &Market, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(m.as_lower())
}

fn serialize_board_lower<S: Serializer>(b: &Option<Board>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(b.map(Board::as_str).unwrap_or(BOARD_UNKNOWN))
}

/// Sentinel string for "board could not be inferred from the code".
/// Used both in JSON serialization and the table renderer so the two
/// outputs stay consistent.
const BOARD_UNKNOWN: &str = "—";

impl RenderRow for SearchHit {
    fn headers() -> &'static [&'static str] {
        &["code", "name", "market", "board", "category", "orgId"]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.code.clone(),
            self.name.clone(),
            // Table is human-facing — upper-case the market label
            // (`cn-a` → `CN-A`); JSON stays lowercase for machines.
            self.market.as_upper().to_string(),
            self.board.map(Board::as_str).unwrap_or(BOARD_UNKNOWN).to_string(),
            self.category.clone(),
            self.org_id.clone(),
        ]
    }
}

/// Entry point dispatched from `main.rs`.
pub fn run(args: SearchArgs, fmt: Format) -> Result<(), SiftError> {
    let http = HttpClient::new();
    let lists = fetch_stock_lists(
        &http,
        SearchCacheOpts {
            no_cache: args.no_cache,
        },
    )?;
    let hits = find_matches(&lists, &args.query, args.limit);
    if hits.is_empty() {
        return Err(SiftError::NotFound(args.query));
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    output::render(&mut handle, fmt, &hits)?;
    Ok(())
}

/// Three-way fuzzy match. The query is normalized (`trim` +
/// `to_ascii_lowercase`); Chinese characters pass through unchanged so
/// `zwjc.contains(query)` stays correct.
pub fn find_matches(lists: &StockLists, query: &str, limit: u32) -> Vec<SearchHit> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return Vec::new();
    }

    let mut matched: Vec<(Market, &CnInfoRow)> = lists
        .cn_a
        .iter()
        .map(|r| (Market::CnA, r))
        .chain(lists.hk.iter().map(|r| (Market::Hk, r)))
        .filter(|(_, r)| matches_any(&q, r))
        .collect();

    // `Market` derives `Ord` with CnA < Hk, so the sort key reads as
    // "A-share before HK, then by code lexicographically".
    matched.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.code.cmp(&b.1.code)));
    matched.truncate(limit as usize);
    matched.into_iter().map(|(m, r)| to_hit(m, r)).collect()
}

/// cninfo guarantees `code` is ASCII digits and `pinyin` is lowercase
/// ASCII initials, so we use raw `contains` / `starts_with` without
/// re-lowercasing per row (which would allocate a new String per row
/// per search). `zwjc` is Chinese — substring match passes through.
fn matches_any(q: &str, r: &CnInfoRow) -> bool {
    r.code.contains(q) || r.zwjc.contains(q) || r.pinyin.starts_with(q)
}

fn to_hit(m: Market, r: &CnInfoRow) -> SearchHit {
    SearchHit {
        code: r.code.clone(),
        name: r.zwjc.clone(),
        pinyin: r.pinyin.clone(),
        category: r.category.clone(),
        org_id: r.org_id.clone(),
        market: m,
        board: infer_board(&r.code),
        source: "cninfo",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(code: &str, zwjc: &str, pinyin: &str, category: &str, org_id: &str) -> CnInfoRow {
        CnInfoRow {
            code: code.into(),
            zwjc: zwjc.into(),
            pinyin: pinyin.into(),
            category: category.into(),
            org_id: org_id.into(),
        }
    }

    fn lists() -> StockLists {
        StockLists {
            cn_a: vec![
                row("600519", "贵州茅台", "gzmt", "A股", "gssh0600519"),
                row("002013", "贵州轮胎", "gzlt", "A股", "gssz0002013"),
                row("000001", "平安银行", "payh", "A股", "gssz0000001"),
                row("601288", "农业银行", "nyyh", "A股", "gssh0601288"),
                row("601398", "工商银行", "gsyh", "A股", "gssh0601398"),
                row("688123", "聚辰股份", "jcgf", "A股", "gssh0688123"),
            ],
            hk: vec![
                row("00388", "香港交易所", "xgjys", "港股", "gshk00388"),
                row("00700", "腾讯控股", "txkg", "港股", "gshk00700"),
            ],
        }
    }

    #[test]
    fn chinese_substring_matches_zwjc() {
        let hits = find_matches(&lists(), "茅台", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].code, "600519");
        assert_eq!(hits[0].name, "贵州茅台");
        assert_eq!(hits[0].market, Market::CnA);
        assert_eq!(hits[0].board, Some(Board::ShMain));
    }

    #[test]
    fn pinyin_initials_match_case_insensitively() {
        let hits = find_matches(&lists(), "gzmt", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].code, "600519");

        // Uppercase / mixed case → same result via `to_ascii_lowercase`.
        let upper = find_matches(&lists(), "GZMT", 10);
        assert_eq!(upper, hits);

        // Multi-row prefix: both `gzmt` and `gzlt` start with `gz`.
        let multi = find_matches(&lists(), "gz", 10);
        let codes: Vec<&str> = multi.iter().map(|h| h.code.as_str()).collect();
        assert!(codes.contains(&"600519"));
        assert!(codes.contains(&"002013"));
        // Sorted lexicographically within market.
        assert_eq!(codes[0], "002013");
    }

    #[test]
    fn code_exact_match_returns_single_row() {
        let hits = find_matches(&lists(), "600519", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].code, "600519");
    }

    #[test]
    fn code_substring_match() {
        let hits = find_matches(&lists(), "688", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].code, "688123");
        assert_eq!(hits[0].board, Some(Board::ShStar));
    }

    #[test]
    fn five_digit_hk_code_returns_none_board() {
        let hits = find_matches(&lists(), "00700", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].code, "00700");
        assert_eq!(hits[0].market, Market::Hk);
        assert_eq!(hits[0].board, None);
    }

    #[test]
    fn empty_or_whitespace_query_returns_empty() {
        assert!(find_matches(&lists(), "", 10).is_empty());
        assert!(find_matches(&lists(), "   ", 10).is_empty());
    }

    #[test]
    fn no_match_returns_empty_vec() {
        assert!(find_matches(&lists(), "absolutely_not_present", 10).is_empty());
    }

    #[test]
    fn sort_puts_a_share_before_hk_then_by_code() {
        // `"00"` is a substring of multiple A-share codes (000001 / 002013 /
        // 600519) as well as both HK codes. Ordering: all A-share rows
        // first by lexicographic code, then HK rows by code.
        let hits = find_matches(&lists(), "00", 10);
        let codes: Vec<&str> = hits.iter().map(|h| h.code.as_str()).collect();
        assert_eq!(codes, vec!["000001", "002013", "600519", "00388", "00700"]);
        let markets: Vec<Market> = hits.iter().map(|h| h.market).collect();
        assert_eq!(
            markets,
            vec![Market::CnA, Market::CnA, Market::CnA, Market::Hk, Market::Hk]
        );
    }

    #[test]
    fn limit_truncates_after_sort() {
        // "银行" hits 000001 / 601288 / 601398.
        let hits = find_matches(&lists(), "银行", 2);
        assert_eq!(hits.len(), 2);
        // Sorted: 000001 wins, then 601288.
        assert_eq!(hits[0].code, "000001");
        assert_eq!(hits[1].code, "601288");
    }

    #[test]
    fn renders_typed_fields_correctly() {
        // Spot-check that the enum → string mapping reaches the renderer
        // unchanged for both branches (Some(board), None).
        let hits = find_matches(&lists(), "茅台", 10);
        let cells = hits[0].cells();
        assert_eq!(cells[2], "CN-A"); // market upper-cased for table
        assert_eq!(cells[3], "sh-main");

        let hk = find_matches(&lists(), "00700", 10);
        let cells = hk[0].cells();
        assert_eq!(cells[2], "HK");
        assert_eq!(cells[3], "—");
    }

    #[test]
    fn json_serializes_market_and_board_lowercase() {
        let hits = find_matches(&lists(), "茅台", 10);
        let v: serde_json::Value = serde_json::to_value(&hits[0]).unwrap();
        assert_eq!(v["market"], "cn-a");
        assert_eq!(v["board"], "sh-main");

        let hk = find_matches(&lists(), "00700", 10);
        let v: serde_json::Value = serde_json::to_value(&hk[0]).unwrap();
        assert_eq!(v["market"], "hk");
        assert_eq!(v["board"], "—");
    }
}
