//! Row type for `sift bars` — one historical K-line bar for one
//! symbol (F5 story-02 + story-03).
//!
//! Schema and unit normalization follow the output table in
//! `docs/f5-realtime/README.md`. `symbol` is the universal code,
//! `date` is ISO `YYYY-MM-DD`, price fields are yuan (divided by
//! 100 from EM's raw cents form when applicable), `volume` is in
//! shares (multiplied by 100 from the upstream "hands" unit), and
//! `adjust` carries `none` / `pre` / `post` as a literal so a flat
//! multi-symbol TSV stays self-describing.
//!
//! `turnover_pct` was intentionally dropped: it requires the
//! current share-outstanding count, which neither Tencent nor EM
//! returns inside the kline payload, so reporting it would require
//! a second per-symbol HTTP call. The remaining derived fields
//! (`amount` / `pct_change` / `change` / `amplitude_pct`) are either
//! reported natively (EM) or computed client-side from OHLCV
//! (Tencent) — see each source for the exact rules.

use serde::Serialize;

use crate::output::RenderRow;

/// Adjustment mode. Literal values line up with the CLI flag
/// `--adjust none|pre|post` and are also stored in `BarRow.adjust`
/// (per-row, so a flat multi-symbol TSV stays self-describing).
/// Each source maps these to its own native parameter (EM's `fqt`
/// or Tencent's `qfq`/`hfq`/`bfq`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Adjust {
    None,
    Pre,
    Post,
}

impl Adjust {
    pub fn as_str(self) -> &'static str {
        match self {
            Adjust::None => "none",
            Adjust::Pre => "pre",
            Adjust::Post => "post",
        }
    }
}

/// Bar period. Only daily / weekly / monthly are supported in the
/// first release — Tencent does not serve quarterly / yearly from
/// the kline endpoint, and we deliberately do not synthesize them
/// via client-side resample (`pandas.DataFrame.resample('Q'/'Y')`
/// is the documented downstream path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Daily,
    Weekly,
    Monthly,
}

impl Period {
    pub fn as_str(self) -> &'static str {
        match self {
            Period::Daily => "daily",
            Period::Weekly => "weekly",
            Period::Monthly => "monthly",
        }
    }
}

/// Canonical input to the bars data layer. Both EM and Tencent
/// share this shape — the `fetch::bars` dispatcher passes the same
/// struct down to whichever [`crate::sources::bars_source::BarsSource`]
/// is selected. Keeping the query in `domain/` (rather than
/// duplicated per source) is the same pattern F2 uses with
/// [`crate::domain::Query`].
#[derive(Debug, Clone, PartialEq)]
pub struct BarsQuery {
    pub symbol: crate::domain::Symbol,
    pub start: Option<time::Date>,
    pub end: Option<time::Date>,
    pub limit: Option<usize>,
    pub adjust: Adjust,
    pub period: Period,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BarRow {
    pub symbol: String,
    pub date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub amount: f64,
    pub pct_change: f64,
    pub change: f64,
    pub amplitude_pct: f64,
    pub adjust: Adjust,
    pub period: Period,
    pub source: &'static str,
}

impl RenderRow for BarRow {
    fn headers() -> &'static [&'static str] {
        &[
            "symbol",
            "date",
            "open",
            "high",
            "low",
            "close",
            "volume",
            "amount",
            "pct_change",
            "change",
            "amplitude_pct",
            "adjust",
            "period",
            "source",
        ]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.symbol.clone(),
            self.date.clone(),
            format!("{:.2}", self.open),
            format!("{:.2}", self.high),
            format!("{:.2}", self.low),
            format!("{:.2}", self.close),
            self.volume.to_string(),
            format!("{:.2}", self.amount),
            format!("{:.2}", self.pct_change),
            format!("{:.2}", self.change),
            format!("{:.2}", self.amplitude_pct),
            self.adjust.as_str().to_string(),
            self.period.as_str().to_string(),
            self.source.to_string(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BarRow {
        BarRow {
            symbol: "600519.CN-A".into(),
            date: "2026-05-14".into(),
            open: 1325.00,
            high: 1340.50,
            low: 1320.10,
            close: 1330.00,
            volume: 358_060_000,
            amount: 4_762_320_000.0,
            pct_change: 0.45,
            change: 5.95,
            amplitude_pct: 1.54,
            adjust: Adjust::Pre,
            period: Period::Daily,
            source: "tencent",
        }
    }

    #[test]
    fn headers_are_fourteen_columns_in_readme_order() {
        let h = BarRow::headers();
        assert_eq!(h.len(), 14);
        assert_eq!(h[0], "symbol");
        assert_eq!(h[1], "date");
        assert_eq!(h[2], "open");
        assert_eq!(h[5], "close");
        assert_eq!(h[11], "adjust");
        assert_eq!(h[12], "period");
        assert_eq!(h[13], "source");
        // turnover_pct was removed in F5 follow-up — no longer in
        // the header set.
        assert!(!h.contains(&"turnover_pct"));
    }

    #[test]
    fn cells_match_headers_and_format() {
        let row = sample();
        let cells = row.cells();
        assert_eq!(cells.len(), BarRow::headers().len());
        assert_eq!(cells[0], "600519.CN-A");
        assert_eq!(cells[1], "2026-05-14");
        assert_eq!(cells[2], "1325.00");
        assert_eq!(cells[5], "1330.00");
        assert_eq!(cells[6], "358060000");
        assert_eq!(cells[11], "pre");
        assert_eq!(cells[12], "daily");
        assert_eq!(cells[13], "tencent");
    }

    #[test]
    fn adjust_as_str_matches_cli_literals() {
        assert_eq!(Adjust::None.as_str(), "none");
        assert_eq!(Adjust::Pre.as_str(), "pre");
        assert_eq!(Adjust::Post.as_str(), "post");
    }

    #[test]
    fn period_as_str_matches_cli_literals() {
        assert_eq!(Period::Daily.as_str(), "daily");
        assert_eq!(Period::Weekly.as_str(), "weekly");
        assert_eq!(Period::Monthly.as_str(), "monthly");
    }
}
