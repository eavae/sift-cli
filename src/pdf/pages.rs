//! Parse the `--pages` spec into a deduped, sorted list of 1-based
//! page numbers. Pure: takes a string, returns a `PageSpec` or a
//! `SiftError::Parse`. Total-page-count clamping is **not** done
//! here — this layer has no idea how long the target PDF is; that
//! validation lives in story-02 once `pdf-oxide` is wired up.
//!
//! Grammar:
//!
//! ```text
//! spec    := segment ("," segment)*
//! segment := N | N "-" M           (whitespace around tokens is trimmed)
//! ```
//!
//! `N` must be a positive (1-based) integer; ranges with `lo > hi`
//! are rejected explicitly so we surface "you wrote 5-1" before the
//! user wonders why nothing extracted.

use crate::error::SiftError;

/// Parsed `--pages` value: always **sorted ascending and deduped**,
/// every element `> 0`. Empty `PageSpec` is unreachable — an empty
/// input string is rejected up front.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSpec(pub Vec<u32>);

impl PageSpec {
    /// Parse a `--pages` argument. See module docs for grammar.
    pub fn parse(s: &str) -> Result<Self, SiftError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(SiftError::Parse(
                "--pages must not be empty".into(),
            ));
        }
        let mut out: Vec<u32> = Vec::new();
        for segment in s.split(',') {
            let segment = segment.trim();
            if segment.is_empty() {
                return Err(SiftError::Parse(
                    "--pages: empty segment between commas".into(),
                ));
            }
            match segment.split_once('-') {
                None => out.push(parse_u32(segment)?),
                Some((lo, hi)) => {
                    let lo = parse_u32(lo)?;
                    let hi = parse_u32(hi)?;
                    if lo > hi {
                        return Err(SiftError::Parse(format!(
                            "--pages: range {lo}-{hi} reversed"
                        )));
                    }
                    out.extend(lo..=hi);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(PageSpec(out))
    }
}

fn parse_u32(s: &str) -> Result<u32, SiftError> {
    let s = s.trim();
    let n: u32 = s.parse().map_err(|_| {
        SiftError::Parse(format!(
            "--pages: expected positive integer, got {s:?}"
        ))
    })?;
    if n == 0 {
        return Err(SiftError::Parse(
            "--pages: page numbers are 1-based".into(),
        ));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(s: &str) -> Vec<u32> {
        PageSpec::parse(s).unwrap().0
    }

    #[test]
    fn single_page() {
        assert_eq!(pages("3"), vec![3]);
    }

    #[test]
    fn range() {
        assert_eq!(pages("1-5"), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn multi_segment() {
        assert_eq!(pages("1-3,7,10-12"), vec![1, 2, 3, 7, 10, 11, 12]);
    }

    #[test]
    fn auto_sorted() {
        assert_eq!(pages("7,1-3"), vec![1, 2, 3, 7]);
    }

    #[test]
    fn dedupes_overlapping_ranges() {
        assert_eq!(pages("1-5,3-7"), vec![1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn reversed_range_rejected() {
        let err = PageSpec::parse("5-1").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("reversed"), "msg: {msg}");
    }

    #[test]
    fn zero_page_rejected() {
        let err = PageSpec::parse("0").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("1-based"), "msg: {msg}");
    }

    #[test]
    fn non_numeric_rejected() {
        let err = PageSpec::parse("abc").unwrap_err();
        // The exact phrasing isn't load-bearing, but it must be a
        // Parse error and mention the bad input.
        assert!(matches!(err, SiftError::Parse(_)));
        assert!(err.to_string().contains("abc"), "msg: {err}");
    }

    #[test]
    fn empty_rejected() {
        let err = PageSpec::parse("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "msg: {err}");
    }

    #[test]
    fn whitespace_tolerated() {
        assert_eq!(pages("  1-3 , 5 "), vec![1, 2, 3, 5]);
    }

    #[test]
    fn zero_in_range_rejected() {
        // Catches an easy mistake — `0-3` would otherwise silently
        // expand to "1-based page 0" which doesn't exist.
        let err = PageSpec::parse("0-3").unwrap_err();
        assert!(err.to_string().contains("1-based"), "msg: {err}");
    }
}
