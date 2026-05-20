//! `Period` — a financial report period.
//!
//! Accepts the user-facing literal forms documented in the F2 README
//! "报告期解析速查" (`2024A` / `2024Q1` / `2024H1` / `2024Q3` /
//! `YYYY-MM-DD`); `2024` (bare year) is explicitly rejected as
//! ambiguous and must be expanded at the command layer into the four
//! standard period-ends.
//!
//! HK F10 uses `DATE_TYPE_CODE` (`001`/`002`/`003`/`004`) instead of
//! the period-end literal; [`Period::from_date_type_code`] folds those
//! back into the same enum so downstream code sees one shape.

use time::{Date, Month};

use crate::error::SiftError;

/// One report period.
///
/// `Custom(Date)` exists for the rare case where a caller knows an
/// exact, non-aligned report-end date (e.g. an interim restatement);
/// `Period::parse` auto-normalizes any aligned `YYYY-MM-DD` to the
/// matching `Annual` / `H1` / `Q1` / `Q3` variant, so user input
/// never reaches `Custom` for the standard four ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Period {
    /// Calendar-year end, `YYYY-12-31`.
    Annual(i32),
    /// First half, `YYYY-06-30`.
    H1(i32),
    /// First quarter, `YYYY-03-31`.
    Q1(i32),
    /// Third quarter, `YYYY-09-30`.
    Q3(i32),
    /// Arbitrary `YYYY-MM-DD` not aligned to one of the four standard ends.
    Custom(Date),
}

/// The four standard report-end shapes (without a year). Stored on a
/// `FinancialRow` so the rendering layer can format `annual` / `q1` /
/// `h1` / `q3` without re-deriving from the `Date`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PeriodType {
    Annual,
    H1,
    Q1,
    Q3,
}

impl PeriodType {
    /// Lower-case string used in `period_type` column / JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Annual => "annual",
            Self::H1 => "h1",
            Self::Q1 => "q1",
            Self::Q3 => "q3",
        }
    }

    /// Derive the period type from a known end-of-period `Date`, if
    /// it aligns to one of the four standard ends.
    pub fn from_date(d: Date) -> Option<Self> {
        match (d.month() as u8, d.day()) {
            (12, 31) => Some(Self::Annual),
            (6, 30) => Some(Self::H1),
            (3, 31) => Some(Self::Q1),
            (9, 30) => Some(Self::Q3),
            _ => None,
        }
    }
}

impl Period {
    /// Parse one of the user-facing literal forms. Case-insensitive on
    /// the suffix; `2024` (bare year) is rejected — the command layer
    /// must expand it into the four standard ends explicitly.
    pub fn parse(input: &str) -> Result<Self, SiftError> {
        let s = input.trim();
        if s.is_empty() {
            return Err(SiftError::Parse("empty period".into()));
        }

        // 1) `YYYY-MM-DD` — auto-normalize aligned ends to the matching variant.
        if let Some(d) = parse_iso_date(s) {
            return Ok(Self::from_date(d));
        }

        // 2) `YYYY` + suffix (`A` / `Q1`-`Q4` / `H1`), case-insensitive.
        let upper = s.to_ascii_uppercase();
        let Some(year_str) = upper.get(..4) else {
            return Err(SiftError::Parse(format!("unrecognized period {input:?}")));
        };
        if !year_str.chars().all(|c| c.is_ascii_digit()) {
            return Err(SiftError::Parse(format!("unrecognized period {input:?}")));
        }
        let year: i32 = year_str
            .parse()
            .map_err(|_| SiftError::Parse(format!("bad year in {input:?}")))?;

        match &upper[4..] {
            "" => Err(SiftError::Parse(format!(
                "ambiguous period {input:?}; use 2024A or expand at command layer"
            ))),
            "A" | "Q4" => Ok(Self::Annual(year)),
            "Q1" => Ok(Self::Q1(year)),
            "Q2" | "H1" => Ok(Self::H1(year)),
            "Q3" => Ok(Self::Q3(year)),
            other => Err(SiftError::Parse(format!(
                "unrecognized period suffix {other:?} in {input:?}"
            ))),
        }
    }

    /// Construct from a `Date`, auto-normalizing aligned ends.
    fn from_date(d: Date) -> Self {
        match PeriodType::from_date(d) {
            Some(PeriodType::Annual) => Self::Annual(d.year()),
            Some(PeriodType::H1) => Self::H1(d.year()),
            Some(PeriodType::Q1) => Self::Q1(d.year()),
            Some(PeriodType::Q3) => Self::Q3(d.year()),
            None => Self::Custom(d),
        }
    }

    /// Resolve to the report-end `Date`.
    pub fn end_date(&self) -> Date {
        match self {
            Self::Annual(y) => date(*y, Month::December, 31),
            Self::H1(y) => date(*y, Month::June, 30),
            Self::Q1(y) => date(*y, Month::March, 31),
            Self::Q3(y) => date(*y, Month::September, 30),
            Self::Custom(d) => *d,
        }
    }

    /// `PeriodType` view — `None` only for a non-aligned `Custom` date.
    pub fn period_type(&self) -> Option<PeriodType> {
        match self {
            Self::Annual(_) => Some(PeriodType::Annual),
            Self::H1(_) => Some(PeriodType::H1),
            Self::Q1(_) => Some(PeriodType::Q1),
            Self::Q3(_) => Some(PeriodType::Q3),
            Self::Custom(d) => PeriodType::from_date(*d),
        }
    }

    /// Reconstruct from the HK F10 `DATE_TYPE_CODE` + report-end date.
    /// Unknown codes return `SiftError::Parse`.
    pub fn from_date_type_code(code: &str, end: Date) -> Result<Self, SiftError> {
        match code {
            "001" => Ok(Self::Annual(end.year())),
            "002" => Ok(Self::H1(end.year())),
            "003" => Ok(Self::Q1(end.year())),
            "004" => Ok(Self::Q3(end.year())),
            other => Err(SiftError::Parse(format!(
                "unknown DATE_TYPE_CODE {other:?}"
            ))),
        }
    }

    /// Step one standard period back: `Annual ← Q3 ← H1 ← Q1 ← Annual(y-1)`.
    /// `Custom` falls back to `Annual(2000)` since the calendar walker
    /// never produces it.
    pub fn previous(self) -> Period {
        match self {
            Period::Annual(y) => Period::Q3(y),
            Period::Q3(y) => Period::H1(y),
            Period::H1(y) => Period::Q1(y),
            Period::Q1(y) => Period::Annual(y - 1),
            Period::Custom(_) => Period::Annual(2000),
        }
    }
}

/// Most-recent standard period that is guaranteed to be filed by
/// `today`. Conservative: uses the upper bound of A-share filing
/// deadlines (Q1 by Apr 30, H1 by Aug 31, Q3 by Oct 31, Annual by
/// next-year Apr 30). When in doubt, returns an earlier period —
/// better to undershoot than to query a period that has not been
/// filed yet.
pub fn most_recent_filed(today: Date) -> Period {
    let y = today.year();
    let m = today.month() as u8;
    match m {
        11..=12 => Period::Q3(y),           // Q3(y) end Sep 30, deadline Oct 31
        9..=10 => Period::H1(y),            // H1(y) end Jun 30, deadline Aug 31
        5..=8 => Period::Q1(y),             // Q1(y) end Mar 31, deadline Apr 30
        _ => Period::Q3(y - 1),             // Jan-Apr: y-1 Q3 is the latest safe
    }
}

/// Produce the most-recent-N period list anchored on today, walking
/// [`most_recent_filed`] backwards via [`Period::previous`]. Returned
/// in newest-first order.
pub fn last_n_filed(today: Date, n: usize) -> Vec<Period> {
    let mut cur = most_recent_filed(today);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(cur);
        cur = cur.previous();
    }
    out
}

fn date(y: i32, m: Month, d: u8) -> Date {
    // All call sites pass real calendar dates (12-31, 6-30, 3-31, 9-30)
    // so `from_calendar_date` never fails. The `expect` documents that
    // invariant for future readers.
    Date::from_calendar_date(y, m, d).expect("standard period-end date is always valid")
}

fn parse_iso_date(s: &str) -> Option<Date> {
    let mut parts = s.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m_num: u8 = parts.next()?.parse().ok()?;
    let d: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let month = Month::try_from(m_num).ok()?;
    Date::from_calendar_date(y, month, d).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_suffix_forms_case_insensitive() {
        assert_eq!(Period::parse("2024A").unwrap(), Period::Annual(2024));
        assert_eq!(Period::parse("2024a").unwrap(), Period::Annual(2024));
        assert_eq!(Period::parse("2024Q4").unwrap(), Period::Annual(2024));
        assert_eq!(Period::parse("2024Q1").unwrap(), Period::Q1(2024));
        assert_eq!(Period::parse("2024q1").unwrap(), Period::Q1(2024));
        assert_eq!(Period::parse("2024Q2").unwrap(), Period::H1(2024));
        assert_eq!(Period::parse("2024H1").unwrap(), Period::H1(2024));
        assert_eq!(Period::parse("2024Q3").unwrap(), Period::Q3(2024));
    }

    #[test]
    fn parse_iso_date_normalizes_aligned_ends() {
        assert_eq!(Period::parse("2024-12-31").unwrap(), Period::Annual(2024));
        assert_eq!(Period::parse("2024-06-30").unwrap(), Period::H1(2024));
        assert_eq!(Period::parse("2024-03-31").unwrap(), Period::Q1(2024));
        assert_eq!(Period::parse("2024-09-30").unwrap(), Period::Q3(2024));
    }

    #[test]
    fn parse_iso_date_keeps_non_aligned_as_custom() {
        let parsed = Period::parse("2024-08-15").unwrap();
        match parsed {
            Period::Custom(d) => {
                assert_eq!(d.year(), 2024);
                assert_eq!(d.month() as u8, 8);
                assert_eq!(d.day(), 15);
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn parse_bare_year_is_ambiguous() {
        let err = Period::parse("2024").unwrap_err();
        match err {
            SiftError::Parse(m) => assert!(m.contains("ambiguous"), "msg: {m}"),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn parse_short_year_or_unknown_suffix_is_parse_error() {
        assert!(matches!(Period::parse("24Q1"), Err(SiftError::Parse(_))));
        assert!(matches!(Period::parse("2024X"), Err(SiftError::Parse(_))));
        assert!(matches!(Period::parse("2024Q5"), Err(SiftError::Parse(_))));
        assert!(matches!(Period::parse("abcd"), Err(SiftError::Parse(_))));
        assert!(matches!(Period::parse(""), Err(SiftError::Parse(_))));
    }

    #[test]
    fn end_date_for_each_variant_2024() {
        assert_eq!(
            Period::Annual(2024).end_date(),
            date(2024, Month::December, 31)
        );
        assert_eq!(Period::H1(2024).end_date(), date(2024, Month::June, 30));
        assert_eq!(Period::Q1(2024).end_date(), date(2024, Month::March, 31));
        assert_eq!(
            Period::Q3(2024).end_date(),
            date(2024, Month::September, 30)
        );
    }

    #[test]
    fn end_date_in_non_leap_year_is_identical_structurally() {
        // Leap behavior would only show up in Feb; our four ends are all
        // outside February, so 2023 vs 2024 are structurally identical.
        for &y in &[2023i32, 2024, 2025] {
            assert_eq!(Period::Annual(y).end_date().year(), y);
            assert_eq!(Period::Annual(y).end_date().day(), 31);
        }
    }

    #[test]
    fn from_date_type_code_maps_hk_codes() {
        let end = date(2024, Month::June, 30);
        assert_eq!(
            Period::from_date_type_code("001", date(2024, Month::December, 31)).unwrap(),
            Period::Annual(2024)
        );
        assert_eq!(
            Period::from_date_type_code("002", end).unwrap(),
            Period::H1(2024)
        );
        assert_eq!(
            Period::from_date_type_code("003", date(2024, Month::March, 31)).unwrap(),
            Period::Q1(2024)
        );
        assert_eq!(
            Period::from_date_type_code("004", date(2024, Month::September, 30)).unwrap(),
            Period::Q3(2024)
        );
        assert!(matches!(
            Period::from_date_type_code("999", end),
            Err(SiftError::Parse(_))
        ));
    }

    #[test]
    fn period_type_for_aligned_custom_is_some() {
        let p = Period::Custom(date(2024, Month::June, 30));
        // Custom only ends up here if someone bypassed parse(); the API
        // is still expected to infer the type when the date is aligned.
        assert_eq!(p.period_type(), Some(PeriodType::H1));
    }

    #[test]
    fn period_type_for_non_aligned_custom_is_none() {
        let p = Period::Custom(date(2024, Month::August, 15));
        assert_eq!(p.period_type(), None);
    }

    #[test]
    fn period_type_as_str_table() {
        assert_eq!(PeriodType::Annual.as_str(), "annual");
        assert_eq!(PeriodType::H1.as_str(), "h1");
        assert_eq!(PeriodType::Q1.as_str(), "q1");
        assert_eq!(PeriodType::Q3.as_str(), "q3");
    }

    #[test]
    fn previous_walks_backwards() {
        assert_eq!(Period::Annual(2025).previous(), Period::Q3(2025));
        assert_eq!(Period::Q3(2025).previous(), Period::H1(2025));
        assert_eq!(Period::H1(2025).previous(), Period::Q1(2025));
        // Q1 → previous year's Annual.
        assert_eq!(Period::Q1(2025).previous(), Period::Annual(2024));
    }

    #[test]
    fn most_recent_filed_per_calendar_month() {
        let d = |m: u8, day: u8| date(2026, Month::try_from(m).unwrap(), day);
        // May–Aug: current year's Q1 is filed.
        assert_eq!(most_recent_filed(d(5, 20)), Period::Q1(2026));
        assert_eq!(most_recent_filed(d(8, 31)), Period::Q1(2026));
        // Sep–Oct: H1 is filed.
        assert_eq!(most_recent_filed(d(9, 1)), Period::H1(2026));
        assert_eq!(most_recent_filed(d(10, 31)), Period::H1(2026));
        // Nov–Dec: Q3 is filed.
        assert_eq!(most_recent_filed(d(11, 1)), Period::Q3(2026));
        assert_eq!(most_recent_filed(d(12, 31)), Period::Q3(2026));
        // Jan–Apr: Annual(y-1) deadline is Apr 30 — back off to Q3(y-1).
        assert_eq!(most_recent_filed(d(1, 15)), Period::Q3(2025));
        assert_eq!(most_recent_filed(d(4, 30)), Period::Q3(2025));
    }

    #[test]
    fn last_n_filed_returns_n_strictly_descending_periods() {
        let today = date(2026, Month::May, 20);
        let p = last_n_filed(today, 5);
        assert_eq!(p.len(), 5);
        // Anchored at Q1(2026) and walking backwards.
        assert_eq!(p[0], Period::Q1(2026));
        for w in p.windows(2) {
            assert!(w[0].end_date() > w[1].end_date(), "{:?}", p);
        }
    }
}
