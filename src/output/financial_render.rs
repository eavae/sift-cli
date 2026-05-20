//! Pivot + render for `sift financials *` output.
//!
//! Layout: **time is the column axis, items are rows** (transposed
//! from the long-form `FinancialRow` list). Each symbol renders as
//! its own block:
//!
//! ```text
//! [603259.SH  药明康德  scope=consolidated  currency=CNY  unit=raw]
//! 财务指标       2026-03-31      2025-12-31      ...
//! 营业总收入     12,435,776,568  45,456,165,774  ...
//! 营业收入       ...             ...             ...
//! ...
//! _period_type   q1              annual          ...
//! _source        sina            eastmoney       ...
//! ```
//!
//! Metadata rows are prefixed with `_` so they visually separate from
//! real financial line items. Multi-symbol output emits one block per
//! symbol separated by a blank line.

#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;

use serde_json::json;
use unicode_width::UnicodeWidthStr;

use crate::domain::{FinancialRow, Statement, Symbol, Unit};
use crate::error::SiftError;
use crate::output::tabular::{render_tabular, TabularView};
use crate::output::Format;

/// One symbol's pivoted data.
#[derive(Debug, Clone)]
pub struct SymbolBlock {
    pub symbol: Symbol,
    pub name: String,
    pub scope: String,
    pub currency: String,
    pub unit: Unit,
    /// Report-end dates in newest-first order.
    pub periods: Vec<time::Date>,
    /// `(item_name, value-per-period)`. `None` means the source did
    /// not return that item for that period.
    pub items: Vec<(String, Vec<Option<f64>>)>,
    /// `_source` row, parallel to `periods` — which source's row won
    /// the dispatch race for that period.
    pub sources: Vec<String>,
}

/// One pivot run = one or more `SymbolBlock`s in input order.
#[derive(Debug, Clone, Default)]
pub struct PivotedTable {
    pub blocks: Vec<SymbolBlock>,
}

/// Pivot a long-form row list into per-symbol blocks.
///
/// - `keep_items = None` → every observed item is included in
///   first-seen order. `Some(filter)` filters and re-orders to match
///   `filter`'s order.
/// - `name_lookup` is consulted when the source returned an empty
///   `name` (the sina path always does). Missing entries stay blank.
pub fn pivot(
    rows: Vec<FinancialRow>,
    keep_items: Option<&[String]>,
    name_lookup: &HashMap<String, String>,
) -> PivotedTable {
    let mut sym_order: Vec<String> = Vec::new();
    let mut accum: HashMap<String, SymbolAccum> = HashMap::new();

    for r in rows {
        if !accum.contains_key(&r.symbol.code) {
            sym_order.push(r.symbol.code.clone());
        }
        let entry = accum.entry(r.symbol.code.clone()).or_insert_with(|| SymbolAccum {
            symbol: r.symbol.clone(),
            name: r.name.clone(),
            scope: r.scope.as_str().into(),
            currency: r.currency.clone(),
            unit: r.unit,
            period_set: BTreeSet::new(),
            period_source: HashMap::new(),
            item_order: Vec::new(),
            item_seen: HashSet::new(),
            values: HashMap::new(),
        });
        if entry.name.is_empty() && !r.name.is_empty() {
            entry.name = r.name.clone();
        }
        entry.period_set.insert(r.period);
        entry
            .period_source
            .entry(r.period)
            .or_insert_with(|| r.source.as_str().into());
        if !entry.item_seen.contains(&r.item) {
            entry.item_seen.insert(r.item.clone());
            entry.item_order.push(r.item.clone());
        }
        entry.values.insert((r.period, r.item.clone()), r.value);
    }

    let mut blocks = Vec::with_capacity(sym_order.len());
    for code in sym_order {
        let acc = accum.remove(&code).unwrap();
        let name = if acc.name.is_empty() {
            name_lookup
                .get(&acc.symbol.code)
                .cloned()
                .unwrap_or_default()
        } else {
            acc.name.clone()
        };
        // Periods newest-first.
        let mut periods: Vec<time::Date> = acc.period_set.into_iter().collect();
        periods.sort_by(|a, b| b.cmp(a));

        // Item order: filter > first-seen; drop items missing from
        // the observed set (so a user-supplied --items name that no
        // source returned does not produce a row of empties).
        let item_names: Vec<String> = match keep_items {
            Some(filter) => filter
                .iter()
                .filter(|i| acc.item_seen.contains(*i))
                .cloned()
                .collect(),
            None => acc.item_order.clone(),
        };

        let items: Vec<(String, Vec<Option<f64>>)> = item_names
            .into_iter()
            .map(|item| {
                let vals: Vec<Option<f64>> = periods
                    .iter()
                    .map(|p| acc.values.get(&(*p, item.clone())).copied())
                    .collect();
                (item, vals)
            })
            .collect();

        let sources: Vec<String> = periods
            .iter()
            .map(|p| acc.period_source.get(p).cloned().unwrap_or_default())
            .collect();

        blocks.push(SymbolBlock {
            symbol: acc.symbol,
            name,
            scope: acc.scope,
            currency: acc.currency,
            unit: acc.unit,
            periods,
            items,
            sources,
        });
    }
    PivotedTable { blocks }
}

/// Internal accumulator used by [`pivot`].
struct SymbolAccum {
    symbol: Symbol,
    name: String,
    scope: String,
    currency: String,
    unit: Unit,
    period_set: BTreeSet<time::Date>,
    /// Source name of the dispatch winner per period.
    period_source: HashMap<time::Date, String>,
    item_order: Vec<String>,
    item_seen: HashSet<String>,
    values: HashMap<(time::Date, String), f64>,
}

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

pub fn render<W: Write>(
    out: &mut W,
    table: &PivotedTable,
    fmt: Format,
) -> Result<(), SiftError> {
    match fmt {
        Format::Table => render_table(out, table),
        Format::Tsv => render_tsv(out, table),
        Format::Ndjson => render_ndjson(out, table),
    }
}

fn render_table<W: Write>(out: &mut W, table: &PivotedTable) -> Result<(), SiftError> {
    for (i, block) in table.blocks.iter().enumerate() {
        if i > 0 {
            writeln!(out).map_err(io_err)?;
        }
        // Two-line block header.
        //
        // Line 1 — identity: `<secucode>  <name>`
        //   Two whitespace-separated fields (split-friendly for awk).
        // Line 2 — metadata: `key=value  key=value  ...  source=...`
        //   Every token is a `key=value` pair; `source=` aggregates
        //   `_source` across periods via `+` (e.g. `eastmoney+sina`)
        //   when the dispatch winners differ. This drops the
        //   `_period_type` row entirely since the column headers
        //   (period-end dates) already imply Q1 / H1 / Q3 / Annual.
        writeln!(
            out,
            "{secucode}  {name}",
            secucode = format_secucode(&block.symbol),
            name = block.name,
        )
        .map_err(io_err)?;
        writeln!(
            out,
            "scope={scope}  currency={currency}  unit={unit}  source={source}",
            scope = block.scope,
            currency = block.currency,
            unit = block.unit.as_str(),
            source = aggregate_sources(&block.sources),
        )
        .map_err(io_err)?;

        // Body table — pure financial-item rows; `_period_type` /
        // `_source` have moved to the header line.
        let mut headers: Vec<String> = vec!["财务指标".into()];
        for p in &block.periods {
            headers.push(format_date(*p));
        }
        let mut rows: Vec<Vec<String>> = Vec::with_capacity(block.items.len());
        for (item, vals) in &block.items {
            let mut row = vec![item.clone()];
            for v in vals {
                row.push(v.map(|n| format_number(n, block.unit)).unwrap_or_default());
            }
            rows.push(row);
        }
        write_aligned(out, &headers, &rows)?;
    }
    Ok(())
}

/// Collapse a per-period `_source` slice into a single header value:
/// unique source names sorted lexicographically and joined with `+`.
/// Matches the `eastmoney+sina` convention from the F2 README.
fn aggregate_sources(sources: &[String]) -> String {
    let unique: std::collections::BTreeSet<&str> = sources.iter().map(String::as_str).collect();
    unique.into_iter().collect::<Vec<_>>().join("+")
}

fn render_tsv<W: Write>(out: &mut W, table: &PivotedTable) -> Result<(), SiftError> {
    render_tabular(out, &FinancialsTsvView::new(table))
}

/// Flatten a [`PivotedTable`] into one TSV row per `(symbol, period)`
/// observation. The first seven columns carry context (`symbol`,
/// `name`, `period`, `source`, `scope`, `currency`, `unit`); the rest
/// are financial items, taken as the union across every symbol in
/// first-seen order. A symbol that does not report a given item gets
/// an empty cell in that column — pandas reads it as NaN.
///
/// Construction is `O(N_items_total)` (one hash insert per item) so
/// `columns()` / `rows()` can stay cheap.
struct FinancialsTsvView<'a> {
    table: &'a PivotedTable,
    item_cols: Vec<String>,
}

/// Context columns emitted before the financial-item columns. Fixed
/// order; downstream tools key on `symbol` + `period`.
const CONTEXT_COLS: &[&str] = &[
    "symbol", "name", "period", "source", "scope", "currency", "unit",
];

impl<'a> FinancialsTsvView<'a> {
    fn new(table: &'a PivotedTable) -> Self {
        let mut seen: HashSet<String> = HashSet::new();
        let mut item_cols: Vec<String> = Vec::new();
        for block in &table.blocks {
            for (item, _) in &block.items {
                if seen.insert(item.clone()) {
                    item_cols.push(item.clone());
                }
            }
        }
        Self { table, item_cols }
    }
}

impl<'a> TabularView for FinancialsTsvView<'a> {
    fn columns(&self) -> Vec<&str> {
        let mut cols = Vec::with_capacity(CONTEXT_COLS.len() + self.item_cols.len());
        cols.extend_from_slice(CONTEXT_COLS);
        for it in &self.item_cols {
            cols.push(it.as_str());
        }
        cols
    }

    fn rows(&self) -> Vec<Vec<String>> {
        let total_rows: usize = self.table.blocks.iter().map(|b| b.periods.len()).sum();
        let mut out: Vec<Vec<String>> = Vec::with_capacity(total_rows);
        for block in &self.table.blocks {
            // Index this block's items by name so the outer item-column
            // loop is O(1) per cell instead of scanning `block.items`.
            let lookup: HashMap<&str, &[Option<f64>]> = block
                .items
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_slice()))
                .collect();
            for (pi, period) in block.periods.iter().enumerate() {
                let mut row: Vec<String> = Vec::with_capacity(CONTEXT_COLS.len() + self.item_cols.len());
                row.push(format_secucode(&block.symbol));
                row.push(block.name.clone());
                row.push(format_date(*period));
                row.push(block.sources.get(pi).cloned().unwrap_or_default());
                row.push(block.scope.clone());
                row.push(block.currency.clone());
                row.push(block.unit.as_str().into());
                for it in &self.item_cols {
                    let cell = lookup
                        .get(it.as_str())
                        .and_then(|vals| vals.get(pi).copied().flatten())
                        .map(|n| format_number(n, block.unit))
                        .unwrap_or_default();
                    row.push(cell);
                }
                out.push(row);
            }
        }
        out
    }
}

fn render_ndjson<W: Write>(out: &mut W, table: &PivotedTable) -> Result<(), SiftError> {
    for block in &table.blocks {
        for (i, p) in block.periods.iter().enumerate() {
            let mut obj = serde_json::Map::new();
            obj.insert("_symbol".into(), json!(format_secucode(&block.symbol)));
            obj.insert("_name".into(), json!(block.name));
            obj.insert("_period".into(), json!(format_date(*p)));
            obj.insert("_scope".into(), json!(block.scope));
            obj.insert("_currency".into(), json!(block.currency));
            obj.insert("_unit".into(), json!(block.unit.as_str()));
            // `_period_type` dropped — derivable from `_period`'s
            // month/day. `_source` stays so machine consumers can
            // attribute each row to its winning upstream.
            obj.insert(
                "_source".into(),
                json!(block.sources.get(i).cloned().unwrap_or_default()),
            );
            for (item, vals) in &block.items {
                if let Some(Some(v)) = vals.get(i) {
                    let n = serde_json::Number::from_f64(*v)
                        .unwrap_or_else(|| serde_json::Number::from(0));
                    obj.insert(item.clone(), serde_json::Value::Number(n));
                }
            }
            serde_json::to_writer(&mut *out, &obj)
                .map_err(|e| SiftError::Internal(format!("ndjson: {e}")))?;
            out.write_all(b"\n").map_err(io_err)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn write_aligned<W: Write>(
    out: &mut W,
    headers: &[String],
    rows: &[Vec<String>],
) -> Result<(), SiftError> {
    let ncols = headers.len();
    let mut widths = vec![0usize; ncols];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = UnicodeWidthStr::width(h.as_str());
    }
    for row in rows {
        for (i, c) in row.iter().enumerate().take(ncols) {
            let w = UnicodeWidthStr::width(c.as_str());
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }
    write_row(out, &widths, headers)?;
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

fn format_secucode(sym: &Symbol) -> String {
    format!("{}.{}", sym.code, sym.market.as_upper())
}

fn format_date(d: time::Date) -> String {
    d.format(&time::format_description::well_known::Iso8601::DATE)
        .unwrap_or_default()
}

/// Format a numeric cell for table / TSV output.
///
/// - `Unit::Raw`: integer-valued cells render with no decimal, otherwise
///   Rust's default `f64` formatting (full precision).
/// - `Unit::Wan` / `Unit::Yi`: always two decimals. Users who pick a
///   scaled unit are signalling "show me round numbers"; rendering
///   `12345.6789万` defeats the point.
///
/// `NaN` always renders as the empty string so pandas reads it as NaN.
fn format_number(v: f64, unit: Unit) -> String {
    if v.is_nan() {
        return String::new();
    }
    match unit {
        Unit::Wan | Unit::Yi => format!("{v:.2}"),
        Unit::Raw => {
            if v.fract() == 0.0 && v.abs() < 1e15 {
                format!("{v:.0}")
            } else {
                format!("{v}")
            }
        }
    }
}

fn io_err(e: std::io::Error) -> SiftError {
    SiftError::Internal(format!("io: {e}"))
}

// ---------------------------------------------------------------------------
// Unit conversion (called by the command layer before pivot)
// ---------------------------------------------------------------------------

/// Apply `--unit` scaling. Cache always stores `Unit::Raw`; the
/// command layer calls this just before rendering. Indicator rows
/// (ratios such as ROE / 毛利率) are pre-percentages and are
/// **not** scaled — only the `unit` label updates so the column
/// header still reflects the user's request.
pub fn apply_unit(rows: Vec<FinancialRow>, unit: Unit) -> Vec<FinancialRow> {
    if matches!(unit, Unit::Raw) {
        return rows;
    }
    let factor = unit.factor();
    rows.into_iter()
        .map(|mut r| {
            if !matches!(r.statement, Statement::Indicator) {
                r.value /= factor;
            }
            r.unit = unit;
            r
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::Market;
    use crate::domain::{
        AuditStatus, FinancialRow, PeriodType, Scope, SourceTag, Statement, Symbol, Unit,
    };
    use time::Month;

    fn d(y: i32, m: u8, day: u8) -> time::Date {
        time::Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap()
    }

    fn row(
        code: &str,
        market: Market,
        name: &str,
        item: &str,
        value: f64,
        period: time::Date,
        source: SourceTag,
    ) -> FinancialRow {
        FinancialRow {
            symbol: Symbol {
                code: code.into(),
                market,
            },
            name: name.into(),
            period,
            period_type: PeriodType::from_date(period).unwrap_or(PeriodType::Annual),
            statement: Statement::Income,
            scope: Scope::Consolidated,
            item: item.into(),
            value,
            unit: Unit::Raw,
            currency: "CNY".into(),
            publish_date: None,
            audit: AuditStatus::Unknown,
            source,
        }
    }

    fn empty_lookup() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn pivot_groups_by_symbol_and_orders_periods_desc() {
        let rows = vec![
            row("600519", Market::CnA, "贵州茅台", "营业总收入", 100.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("600519", Market::CnA, "贵州茅台", "归母净利润", 50.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("600519", Market::CnA, "贵州茅台", "营业总收入", 80.0, d(2025, 12, 31), SourceTag::EastMoney),
            row("600519", Market::CnA, "贵州茅台", "归母净利润", 40.0, d(2025, 12, 31), SourceTag::EastMoney),
        ];
        let t = pivot(rows, None, &empty_lookup());
        assert_eq!(t.blocks.len(), 1);
        let b = &t.blocks[0];
        assert_eq!(b.symbol.code, "600519");
        assert_eq!(b.name, "贵州茅台");
        assert_eq!(b.periods, vec![d(2025, 12, 31), d(2024, 12, 31)]);
        // Items in first-seen order.
        assert_eq!(b.items[0].0, "营业总收入");
        assert_eq!(b.items[0].1, vec![Some(80.0), Some(100.0)]);
        assert_eq!(b.items[1].0, "归母净利润");
        assert_eq!(b.items[1].1, vec![Some(40.0), Some(50.0)]);
    }

    #[test]
    fn pivot_backfills_name_from_lookup_when_source_left_it_blank() {
        let mut lookup = HashMap::new();
        lookup.insert("600519".into(), "贵州茅台".into());
        let rows = vec![row(
            "600519",
            Market::CnA,
            "", // sina returns blank
            "营业总收入",
            100.0,
            d(2024, 12, 31),
            SourceTag::Sina,
        )];
        let t = pivot(rows, None, &lookup);
        assert_eq!(t.blocks[0].name, "贵州茅台");
    }

    #[test]
    fn pivot_keeps_non_blank_name_from_source_over_lookup() {
        let mut lookup = HashMap::new();
        lookup.insert("600519".into(), "WRONG".into());
        let rows = vec![row(
            "600519",
            Market::CnA,
            "贵州茅台",
            "营业总收入",
            100.0,
            d(2024, 12, 31),
            SourceTag::EastMoney,
        )];
        let t = pivot(rows, None, &lookup);
        assert_eq!(t.blocks[0].name, "贵州茅台");
    }

    #[test]
    fn pivot_groups_multiple_symbols_in_input_order() {
        let rows = vec![
            row("600519", Market::CnA, "茅台", "x", 1.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("000858", Market::CnA, "五粮液", "x", 2.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("600519", Market::CnA, "茅台", "x", 3.0, d(2025, 12, 31), SourceTag::EastMoney),
        ];
        let t = pivot(rows, None, &empty_lookup());
        assert_eq!(t.blocks.len(), 2);
        assert_eq!(t.blocks[0].symbol.code, "600519");
        assert_eq!(t.blocks[1].symbol.code, "000858");
        // 600519 has 2 periods, 000858 has 1.
        assert_eq!(t.blocks[0].periods.len(), 2);
        assert_eq!(t.blocks[1].periods.len(), 1);
    }

    #[test]
    fn render_table_two_line_header_and_no_metadata_rows() {
        let rows = vec![row(
            "600519",
            Market::CnA,
            "贵州茅台",
            "营业总收入",
            100.0,
            d(2024, 12, 31),
            SourceTag::EastMoney,
        )];
        let t = pivot(rows, None, &empty_lookup());
        let mut buf = Vec::<u8>::new();
        render(&mut buf, &t, Format::Table).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        // Line 0: identity (secucode + name, whitespace-separated).
        assert!(lines[0].starts_with("600519.CN-A"));
        assert!(lines[0].contains("贵州茅台"));
        // Line 1: key=value metadata including aggregated source.
        assert!(lines[1].starts_with("scope=consolidated"));
        assert!(lines[1].contains("currency=CNY"));
        assert!(lines[1].contains("unit=raw"));
        assert!(lines[1].contains("source=eastmoney"));
        // Line 2: column header for the body table.
        assert!(lines[2].starts_with("财务指标"));
        assert!(lines[2].contains("2024-12-31"));
        // Body rows: just the items, no `_period_type` / `_source`.
        assert!(s.contains("营业总收入"));
        assert!(!s.contains("_period_type"));
        assert!(!s.contains("_source\t")); // _source is no longer a row prefix
        // Source appears only in the header line, not as a body row.
        let source_hits: usize = lines.iter().filter(|l| l.contains("eastmoney")).count();
        assert_eq!(source_hits, 1, "source label appears exactly once (header)");
    }

    #[test]
    fn render_table_aggregates_mixed_sources_with_plus() {
        let rows = vec![
            row("600519", Market::CnA, "茅台", "营业总收入", 100.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("600519", Market::CnA, "茅台", "营业总收入", 80.0, d(2025, 12, 31), SourceTag::Sina),
        ];
        let t = pivot(rows, None, &empty_lookup());
        let mut buf = Vec::<u8>::new();
        render(&mut buf, &t, Format::Table).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        // Sorted lex: "eastmoney" < "sina"
        assert!(
            lines[1].contains("source=eastmoney+sina"),
            "meta line: {}",
            lines[1]
        );
    }

    #[test]
    fn render_table_separates_multi_symbol_blocks_with_blank_line() {
        let rows = vec![
            row("600519", Market::CnA, "茅台", "x", 1.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("000858", Market::CnA, "五粮液", "x", 2.0, d(2024, 12, 31), SourceTag::EastMoney),
        ];
        let t = pivot(rows, None, &empty_lookup());
        let mut buf = Vec::<u8>::new();
        render(&mut buf, &t, Format::Table).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // First header line of each block is the secucode + name; find
        // them, then verify the line preceding the second header is
        // blank (block separator).
        let lines: Vec<&str> = s.lines().collect();
        let header_positions: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with("600519.CN-A") || l.starts_with("000858.CN-A"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(header_positions.len(), 2);
        assert!(lines[header_positions[1] - 1].is_empty());
    }

    fn render_tsv_string(rows: Vec<FinancialRow>) -> String {
        let t = pivot(rows, None, &empty_lookup());
        let mut buf = Vec::<u8>::new();
        render(&mut buf, &t, Format::Tsv).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn tsv_header_is_canonical_hash_prefix_with_context_then_item_columns() {
        let s = render_tsv_string(vec![row(
            "600519",
            Market::CnA,
            "贵州茅台",
            "营业总收入",
            100.0,
            d(2024, 12, 31),
            SourceTag::Sina,
        )]);
        let header = s.lines().next().unwrap();
        // bio-ncbi convention: no space after `#`.
        assert!(!header.starts_with("# "), "got: {header}");
        assert_eq!(
            header,
            "#symbol\tname\tperiod\tsource\tscope\tcurrency\tunit\t营业总收入"
        );
    }

    #[test]
    fn tsv_emits_one_row_per_symbol_period_pair_with_context_cells() {
        let s = render_tsv_string(vec![
            row("600519", Market::CnA, "贵州茅台", "营业总收入", 100.0, d(2024, 12, 31), SourceTag::Sina),
            row("600519", Market::CnA, "贵州茅台", "归母净利润", 50.0, d(2024, 12, 31), SourceTag::Sina),
            row("600519", Market::CnA, "贵州茅台", "营业总收入", 80.0, d(2025, 12, 31), SourceTag::Sina),
            row("600519", Market::CnA, "贵州茅台", "归母净利润", 40.0, d(2025, 12, 31), SourceTag::Sina),
        ]);
        let lines: Vec<&str> = s.lines().collect();
        // 1 header + 2 data rows, newest first.
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[1],
            "600519.CN-A\t贵州茅台\t2025-12-31\tsina\tconsolidated\tCNY\traw\t80\t40"
        );
        assert_eq!(
            lines[2],
            "600519.CN-A\t贵州茅台\t2024-12-31\tsina\tconsolidated\tCNY\traw\t100\t50"
        );
    }

    #[test]
    fn tsv_multi_symbol_is_single_table_no_blank_separator() {
        // Two symbols, two periods each → 1 header + 4 data rows.
        // Critical: no blank line between symbol blocks.
        let s = render_tsv_string(vec![
            row("600519", Market::CnA, "茅台", "营业总收入", 1.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("600519", Market::CnA, "茅台", "营业总收入", 2.0, d(2025, 12, 31), SourceTag::EastMoney),
            row("000858", Market::CnA, "五粮液", "营业总收入", 3.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("000858", Market::CnA, "五粮液", "营业总收入", 4.0, d(2025, 12, 31), SourceTag::EastMoney),
        ]);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 5);
        for l in &lines[1..] {
            assert!(!l.is_empty(), "no blank separators allowed");
        }
        // Symbols emit in input order; within each, periods newest-first.
        assert!(lines[1].starts_with("600519.CN-A\t茅台\t2025-12-31"));
        assert!(lines[2].starts_with("600519.CN-A\t茅台\t2024-12-31"));
        assert!(lines[3].starts_with("000858.CN-A\t五粮液\t2025-12-31"));
        assert!(lines[4].starts_with("000858.CN-A\t五粮液\t2024-12-31"));
    }

    #[test]
    fn tsv_item_columns_are_union_in_first_seen_order_across_symbols() {
        // 600519 reports A then B; 000001 (bank) reports C (bank-only).
        // Union order = [A, B, C]; the bank's A/B cells are empty, and
        // 600519's C cell is empty.
        let s = render_tsv_string(vec![
            row("600519", Market::CnA, "茅台", "营业总收入", 100.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("600519", Market::CnA, "茅台", "归母净利润", 50.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("000001", Market::CnA, "平安银行", "利息净收入", 999.0, d(2024, 12, 31), SourceTag::EastMoney),
        ]);
        let lines: Vec<&str> = s.lines().collect();
        let header = lines[0];
        assert!(header.ends_with("\t营业总收入\t归母净利润\t利息净收入"), "got: {header}");
        // 600519 row: filled A/B, empty C.
        let r1: Vec<&str> = lines[1].split('\t').collect();
        assert_eq!(*r1.last().unwrap(), ""); // 利息净收入 empty
        assert_eq!(r1[r1.len() - 2], "50"); // 归母净利润
        assert_eq!(r1[r1.len() - 3], "100"); // 营业总收入
        // bank row: empty A/B, filled C.
        let r2: Vec<&str> = lines[2].split('\t').collect();
        assert_eq!(r2[r2.len() - 3], ""); // 营业总收入 empty
        assert_eq!(r2[r2.len() - 2], ""); // 归母净利润 empty
        assert_eq!(*r2.last().unwrap(), "999"); // 利息净收入
    }

    #[test]
    fn tsv_source_column_is_per_row_not_aggregated() {
        // auto mode might pick different sources per period; the TSV
        // `source` cell carries the per-row truth, never aggregated.
        let s = render_tsv_string(vec![
            row("600519", Market::CnA, "茅台", "营业总收入", 100.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("600519", Market::CnA, "茅台", "营业总收入", 80.0, d(2025, 12, 31), SourceTag::Sina),
        ]);
        let lines: Vec<&str> = s.lines().collect();
        // Newest first → row 1 (2025) is sina, row 2 (2024) is eastmoney.
        assert!(lines[1].contains("\tsina\t"), "row 1: {}", lines[1]);
        assert!(lines[2].contains("\teastmoney\t"), "row 2: {}", lines[2]);
        // No aggregated `+` artifact anywhere.
        assert!(!s.contains("eastmoney+sina"));
    }

    #[test]
    fn render_ndjson_emits_one_object_per_period_with_underscore_keys() {
        let rows = vec![
            row("600519", Market::CnA, "茅台", "营业总收入", 100.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("600519", Market::CnA, "茅台", "营业总收入", 80.0, d(2025, 12, 31), SourceTag::Sina),
        ];
        let t = pivot(rows, None, &empty_lookup());
        let mut buf = Vec::<u8>::new();
        render(&mut buf, &t, Format::Ndjson).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::Deserializer::from_slice(&buf)
            .into_iter::<serde_json::Value>()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(parsed.len(), 2);
        // Newest first.
        assert_eq!(parsed[0]["_period"], "2025-12-31");
        assert_eq!(parsed[0]["_source"], "sina");
        assert_eq!(parsed[0]["营业总收入"], 80.0);
        assert_eq!(parsed[1]["_period"], "2024-12-31");
        assert_eq!(parsed[1]["_source"], "eastmoney");
        // Both objects share metadata keys.
        assert_eq!(parsed[0]["_symbol"], "600519.CN-A");
        assert_eq!(parsed[0]["_name"], "茅台");
        assert_eq!(parsed[0]["_scope"], "consolidated");
        assert_eq!(parsed[0]["_currency"], "CNY");
        assert_eq!(parsed[0]["_unit"], "raw");
        // `_period_type` intentionally absent — derivable from `_period`.
        assert!(parsed[0].get("_period_type").is_none());
    }

    #[test]
    fn keep_items_filter_restricts_and_orders_rows() {
        let rows = vec![
            row("x", Market::CnA, "n", "营业总收入", 1.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("x", Market::CnA, "n", "营业成本", 2.0, d(2024, 12, 31), SourceTag::EastMoney),
            row("x", Market::CnA, "n", "归母净利润", 3.0, d(2024, 12, 31), SourceTag::EastMoney),
        ];
        let want = vec!["归母净利润".to_string(), "营业总收入".to_string()];
        let t = pivot(rows, Some(&want), &empty_lookup());
        let names: Vec<&str> = t.blocks[0].items.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["归母净利润", "营业总收入"]);
    }

    #[test]
    fn apply_unit_scales_non_indicator_values() {
        let rows = vec![row(
            "x",
            Market::CnA,
            "n",
            "营业总收入",
            100_000_000.0,
            d(2024, 12, 31),
            SourceTag::EastMoney,
        )];
        let yi = apply_unit(rows, Unit::Yi);
        assert_eq!(yi[0].value, 1.0);
        assert_eq!(yi[0].unit, Unit::Yi);
    }

    #[test]
    fn format_number_raw_keeps_integer_form_and_full_precision() {
        // Raw integer-valued: no trailing `.0`.
        assert_eq!(format_number(12345.0, Unit::Raw), "12345");
        // Raw fractional: default `f64` formatting.
        assert_eq!(format_number(12345.6789, Unit::Raw), "12345.6789");
        // NaN → empty cell so pandas reads as NaN.
        assert_eq!(format_number(f64::NAN, Unit::Raw), "");
    }

    #[test]
    fn format_number_wan_and_yi_always_two_decimals() {
        // The whole point of the rule: users picking a scaled unit want
        // tidy 2-decimal output, not 8-digit trailing noise.
        assert_eq!(format_number(12345.6789, Unit::Wan), "12345.68");
        assert_eq!(format_number(12345.6789, Unit::Yi), "12345.68");
        // Even integer-valued doubles get `.00` under wan/yi for
        // column alignment.
        assert_eq!(format_number(7.0, Unit::Wan), "7.00");
        assert_eq!(format_number(7.0, Unit::Yi), "7.00");
        // NaN still empty.
        assert_eq!(format_number(f64::NAN, Unit::Yi), "");
    }

    #[test]
    fn tsv_under_unit_yi_renders_two_decimal_cells() {
        // End-to-end through TSV: scaled values must come out 2-decimal.
        // 1_234_567_890_000 raw → 12_345.6789 億 → "12345.68".
        let rows = vec![row(
            "600519",
            Market::CnA,
            "茅台",
            "营业总收入",
            1_234_567_890_000.0,
            d(2024, 12, 31),
            SourceTag::EastMoney,
        )];
        let scaled = apply_unit(rows, Unit::Yi);
        let t = pivot(scaled, None, &empty_lookup());
        let mut buf = Vec::<u8>::new();
        render(&mut buf, &t, Format::Tsv).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Header is one line; the data row ends with the formatted value.
        assert!(s.contains("\t12345.68\n"), "tsv output:\n{s}");
    }

    #[test]
    fn table_under_unit_yi_renders_two_decimal_cells() {
        let rows = vec![row(
            "600519",
            Market::CnA,
            "茅台",
            "营业总收入",
            1_234_567_890_000.0,
            d(2024, 12, 31),
            SourceTag::EastMoney,
        )];
        let scaled = apply_unit(rows, Unit::Yi);
        let t = pivot(scaled, None, &empty_lookup());
        let mut buf = Vec::<u8>::new();
        render(&mut buf, &t, Format::Table).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("12345.68"), "table output:\n{s}");
    }

    #[test]
    fn apply_unit_skips_indicator_rows() {
        let mut r = row(
            "x",
            Market::CnA,
            "n",
            "ROE",
            28.5,
            d(2024, 12, 31),
            SourceTag::EastMoney,
        );
        r.statement = Statement::Indicator;
        let yi = apply_unit(vec![r], Unit::Yi);
        assert_eq!(yi[0].value, 28.5);
        assert_eq!(yi[0].unit, Unit::Yi);
    }
}
