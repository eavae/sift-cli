//! Row type for `sift quote` — one snapshot of current price for one
//! symbol.
//!
//! The `RenderRow` impl carries the table / TSV column layout;
//! `Serialize` drives the NDJSON renderer (`--format json`).

use serde::Serialize;

use crate::output::RenderRow;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuoteRow {
    pub symbol: String,
    pub name: String,
    pub price: f64,
    pub change: f64,
    pub pct_change: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub prev_close: f64,
    pub volume: i64,
    pub amount: f64,
    pub time: String,
    pub source: &'static str,
}

impl RenderRow for QuoteRow {
    fn headers() -> &'static [&'static str] {
        &[
            "symbol",
            "name",
            "price",
            "change",
            "pct_change",
            "open",
            "high",
            "low",
            "prev_close",
            "volume",
            "amount",
            "time",
            "source",
        ]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.symbol.clone(),
            self.name.clone(),
            format!("{:.2}", self.price),
            format!("{:.2}", self.change),
            format!("{:.2}", self.pct_change),
            format!("{:.2}", self.open),
            format!("{:.2}", self.high),
            format!("{:.2}", self.low),
            format!("{:.2}", self.prev_close),
            self.volume.to_string(),
            format!("{:.2}", self.amount),
            self.time.clone(),
            self.source.to_string(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> QuoteRow {
        QuoteRow {
            symbol: "600519.SH".into(),
            name: "贵州茅台".into(),
            price: 1323.59,
            change: -7.10,
            pct_change: -0.53,
            open: 1321.00,
            high: 1332.99,
            low: 1320.00,
            prev_close: 1330.69,
            volume: 1_267_432,
            amount: 1_680_717_318.0,
            time: "2026-05-20 15:00:00".into(),
            source: "eastmoney",
        }
    }

    #[test]
    fn headers_are_thirteen_columns_in_readme_order() {
        let h = QuoteRow::headers();
        assert_eq!(h.len(), 13);
        assert_eq!(h[0], "symbol");
        assert_eq!(h[12], "source");
    }

    #[test]
    fn cells_match_header_count_and_format() {
        let row = sample();
        let cells = row.cells();
        assert_eq!(cells.len(), QuoteRow::headers().len());
        assert_eq!(cells[0], "600519.SH");
        assert_eq!(cells[2], "1323.59");
        assert_eq!(cells[3], "-7.10");
        assert_eq!(cells[9], "1267432"); // volume integer, no decimal
        assert_eq!(cells[12], "eastmoney");
    }
}
