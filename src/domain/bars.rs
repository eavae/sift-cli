//! Row type for `sift bars` — one daily K-line bar for one symbol
//! (F5 story-02).
//!
//! Same shape as [`crate::domain::quote::QuoteRow`]: plain data plus
//! a `RenderRow` impl. Schema, column order, and unit normalization
//! follow the output schema table in `docs/f5-realtime/README.md` —
//! `symbol` is the universal code, `date` is ISO `YYYY-MM-DD`, price
//! fields are already divided by 100 (yuan), `volume` is multiplied
//! by 100 (shares), and `adjust` takes the literals
//! `none` / `pre` / `post`.

use serde::Serialize;

use crate::output::RenderRow;

/// Adjustment mode. Literal values line up with the CLI flag
/// `--adjust none|pre|post` and are also stored in `BarRow.adjust`
/// (per-row, so a flat multi-symbol TSV stays self-describing). The
/// `fqt` mapping for EM lives in `sources::eastmoney::bars`.
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
    pub turnover_pct: f64,
    pub adjust: Adjust,
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
            "turnover_pct",
            "adjust",
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
            format!("{:.2}", self.turnover_pct),
            self.adjust.as_str().to_string(),
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
            turnover_pct: 0.29,
            adjust: Adjust::Pre,
            source: "eastmoney",
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
        assert_eq!(h[12], "adjust");
        assert_eq!(h[13], "source");
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
        assert_eq!(cells[12], "pre");
        assert_eq!(cells[13], "eastmoney");
    }

    #[test]
    fn adjust_as_str_matches_cli_literals() {
        assert_eq!(Adjust::None.as_str(), "none");
        assert_eq!(Adjust::Pre.as_str(), "pre");
        assert_eq!(Adjust::Post.as_str(), "post");
    }
}
