//! Multi-symbol grouped TTY renderer for `sift bars`.
//!
//! This module sits next to `output/financial_render.rs` (wide pivot)
//! and `output/announce.rs` (static views) — one output module per
//! command, with the command layer calling only the public entry
//! exposed here.
//!
//! The TSV path does **not** go through this module:
//! [`commands/bars.rs`] dispatches `fmt == Tsv` straight to
//! `output::render(fmt, &all_rows)`, producing the same 14-column
//! flat layout as the single-symbol case. This module only owns the
//! `fmt == Table` human-readable form.
//!
//! Layout (from README "terminal bars (multi-symbol)" section):
//!
//! ```text
//! ── 600519.CN-A  adjust=pre  source=eastmoney ───
//! date        open    high    low     close   volume   amount    pct_change  change  amplitude_pct  turnover_pct
//! 2024-01-15  10.00   20.00   8.00    15.00   10000    1000.00   2.00        3.00    1.00           0.10
//!
//! ── 000858.CN-A  adjust=pre  source=eastmoney ───
//! ...
//! ```
//!
//! Design notes:
//! - Groups follow the input symbol order (a `Vec<symbol>` plus
//!   `HashMap` is used instead of `BTreeMap`, which would sort by
//!   key and lose the user's intent).
//! - Per-row columns drop `symbol` / `adjust` / `source` — those
//!   facts have been hoisted into the group header so the body
//!   stays narrow enough not to wrap.
//! - Each group computes its own column widths; the renderer does
//!   **not** align columns across groups (README is explicit about
//!   this).

use std::collections::HashMap;
use std::io::Write;

use unicode_width::UnicodeWidthStr;

use crate::domain::bars::BarRow;
use crate::error::SiftError;
use crate::output::io_err;

const GROUP_HEADERS: &[&str] = &[
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
];

/// Render multi-symbol bars as one aligned table per symbol. `rows`
/// arrives already concatenated by the caller (multi-symbol flat);
/// this function re-groups by the `symbol` field while preserving
/// each group's first-appearance order.
pub fn render_grouped<W: Write>(out: &mut W, rows: &[BarRow]) -> Result<(), SiftError> {
    let (order, groups) = group_by_symbol(rows);
    for (i, sym) in order.iter().enumerate() {
        if i > 0 {
            writeln!(out).map_err(io_err)?;
        }
        let g = &groups[sym];
        let head = g.first().expect("group is non-empty by construction");
        // Group header carries the per-symbol facts hoisted out of
        // the data rows: `adjust`, `period`, and `source`. The data
        // rows themselves stay narrow (10 columns: date + OHLC +
        // volume + amount + the three diff fields).
        writeln!(
            out,
            "── {sym}  period={per}  adjust={adj}  source={src} ───",
            per = head.period.as_str(),
            adj = head.adjust.as_str(),
            src = head.source,
        )
        .map_err(io_err)?;

        let body: Vec<Vec<String>> = g.iter().map(|r| row_subset(r)).collect();
        write_aligned(out, GROUP_HEADERS, &body)?;
    }
    Ok(())
}

fn group_by_symbol(rows: &[BarRow]) -> (Vec<String>, HashMap<String, Vec<&BarRow>>) {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&BarRow>> = HashMap::new();
    for r in rows {
        if !groups.contains_key(&r.symbol) {
            order.push(r.symbol.clone());
        }
        groups.entry(r.symbol.clone()).or_default().push(r);
    }
    (order, groups)
}

/// Project a [`BarRow`] into the 11-column subset emitted under the
/// group header (i.e. everything except `symbol` / `adjust` /
/// `source`). The order matches [`GROUP_HEADERS`] exactly.
fn row_subset(r: &BarRow) -> Vec<String> {
    vec![
        r.date.clone(),
        format!("{:.2}", r.open),
        format!("{:.2}", r.high),
        format!("{:.2}", r.low),
        format!("{:.2}", r.close),
        r.volume.to_string(),
        format!("{:.2}", r.amount),
        format!("{:.2}", r.pct_change),
        format!("{:.2}", r.change),
        format!("{:.2}", r.amplitude_pct),
    ]
}

/// Column-aligned writer. Same shape as
/// `output::financial_render::write_aligned` and
/// `output::render::render_table`. The three copies live separately
/// today because each variant pulls its columns from a different
/// source (dynamic / static `RenderRow` / static literals); the
/// behaviour contract (no trailing padding on the last column, two-
/// space inter-column gap, Unicode column widths) is shared but
/// there is no common helper extracted yet.
fn write_aligned<W: Write>(out: &mut W, headers: &[&str], rows: &[Vec<String>]) -> Result<(), SiftError> {
    let ncols = headers.len();
    let mut widths = vec![0usize; ncols];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = UnicodeWidthStr::width(*h);
    }
    for row in rows {
        for (i, c) in row.iter().enumerate().take(ncols) {
            let w = UnicodeWidthStr::width(c.as_str());
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }
    write_row(out, &widths, headers.iter().map(|s| s.to_string()).collect::<Vec<_>>().as_slice())?;
    for row in rows {
        write_row(out, &widths, row)?;
    }
    Ok(())
}

fn write_row<W: Write>(
    out: &mut W,
    widths: &[usize],
    cells: &[String],
) -> Result<(), SiftError> {
    let last = cells.len().saturating_sub(1);
    for (i, cell) in cells.iter().enumerate() {
        if i == last {
            out.write_all(cell.as_bytes()).map_err(io_err)?;
        } else {
            out.write_all(cell.as_bytes()).map_err(io_err)?;
            let pad = widths
                .get(i)
                .copied()
                .unwrap_or(0)
                .saturating_sub(UnicodeWidthStr::width(cell.as_str()));
            for _ in 0..pad {
                out.write_all(b" ").map_err(io_err)?;
            }
            out.write_all(b"  ").map_err(io_err)?;
        }
    }
    out.write_all(b"\n").map_err(io_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::bars::{Adjust, Period};

    fn bar(sym: &str, date: &str) -> BarRow {
        BarRow {
            symbol: sym.into(),
            date: date.into(),
            open: 10.0,
            high: 20.0,
            low: 8.0,
            close: 15.0,
            volume: 10_000,
            amount: 1000.0,
            pct_change: 2.0,
            change: 3.0,
            amplitude_pct: 1.0,
            adjust: Adjust::Pre,
            period: Period::Daily,
            source: "tencent",
        }
    }

    #[test]
    fn groups_in_input_order_not_alphabetical() {
        // Input has 000858 before 600519 — alphabetical sort would
        // flip them, but we must preserve input order.
        let rows = vec![
            bar("000858.CN-A", "2024-01-15"),
            bar("000858.CN-A", "2024-01-16"),
            bar("600519.CN-A", "2024-01-15"),
        ];
        let mut buf = Vec::<u8>::new();
        render_grouped(&mut buf, &rows).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let pos858 = s.find("000858.CN-A").unwrap();
        let pos600 = s.find("600519.CN-A").unwrap();
        assert!(pos858 < pos600, "input order preserved:\n{s}");
    }

    #[test]
    fn group_header_carries_period_adjust_and_source() {
        let rows = vec![bar("600519.CN-A", "2024-01-15")];
        let mut buf = Vec::<u8>::new();
        render_grouped(&mut buf, &rows).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("── 600519.CN-A  period=daily  adjust=pre  source=tencent ───"),
            "got:\n{s}"
        );
    }

    #[test]
    fn data_rows_drop_symbol_adjust_source_columns() {
        let rows = vec![bar("600519.CN-A", "2024-01-15")];
        let mut buf = Vec::<u8>::new();
        render_grouped(&mut buf, &rows).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // The data row contains date / OHLC but no `symbol`
        // literal (the symbol only appears once in the group
        // header).
        let data_line = s.lines().last().unwrap();
        assert!(!data_line.contains("600519"), "symbol column leaked into row: {data_line:?}");
        assert!(!data_line.contains("tencent"), "source column leaked: {data_line:?}");
        assert!(!data_line.contains("pre "), "adjust column leaked: {data_line:?}");
        // Date and numbers are present.
        assert!(data_line.contains("2024-01-15"));
        assert!(data_line.contains("10.00"));
    }

    #[test]
    fn multiple_groups_separated_by_blank_line() {
        let rows = vec![
            bar("600519.CN-A", "2024-01-15"),
            bar("000858.CN-A", "2024-01-15"),
        ];
        let mut buf = Vec::<u8>::new();
        render_grouped(&mut buf, &rows).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Groups are separated by at least one blank line.
        assert!(s.contains("\n\n"), "expected blank line between groups:\n{s}");
    }

    #[test]
    fn empty_input_writes_nothing() {
        let mut buf = Vec::<u8>::new();
        render_grouped(&mut buf, &[]).unwrap();
        assert!(buf.is_empty());
    }
}
