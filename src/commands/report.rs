//! `sift report {income | balance | cashflow | indicator | periods}`.

use clap::{Args, Subcommand, ValueEnum};

use crate::domain::market::Market;
use crate::domain::period::last_n_filed;
use crate::domain::{
    items_dict, Period, Query, Scope, SourceTag, Statement, Symbol, Unit,
};
use crate::error::SiftError;
use crate::fetch::report::{
    dispatch_with_cache_named, list_periods_union, load_listing_names, ReportContext,
};
use crate::output::financial_render::{self, pivot};
use crate::output::Format;

/// CLI surface for `sift report`.
#[derive(Subcommand, Debug)]
pub enum ReportCmd {
    /// income statement
    Income(StatementArgs),
    /// balance sheet
    Balance(StatementArgs),
    /// cashflow statement
    Cashflow(StatementArgs),
    /// key indicators
    Indicator(StatementArgs),
    /// List available report periods for a symbol
    Periods(PeriodsArgs),
}

#[derive(Args, Debug)]
pub struct StatementArgs {
    /// One or more symbols (e.g. `600519`, `00700`).
    #[arg(required = true)]
    pub symbols: Vec<String>,

    /// `2024A` / `2024Q1` / `2024H1` / `2024Q3` / `YYYY-MM-DD`,
    /// comma-separated. `2024` (bare year) expands to all four
    /// standard ends of that year.
    #[arg(long, value_delimiter = ',', value_parser = parse_period_token)]
    pub period: Vec<Vec<Period>>,

    /// Most-recent N periods. Mutually exclusive with `--period`.
    #[arg(long)]
    pub last: Option<usize>,

    /// Range start year (`YYYY`); pairs with `--end` + `--annual`.
    #[arg(long)]
    pub start: Option<i32>,

    /// Range end year (`YYYY`); pairs with `--start` + `--annual`.
    #[arg(long)]
    pub end: Option<i32>,

    /// With `--start/--end`, restrict to annual periods only.
    #[arg(long)]
    pub annual: bool,

    /// Consolidated (default) vs Parent-only. Parent is A-share only.
    #[arg(long, value_enum, default_value_t = ScopeArg::Consolidated)]
    pub scope: ScopeArg,

    /// Display unit. Cache always stores raw values.
    #[arg(long, value_enum, default_value_t = UnitArg::Raw)]
    pub unit: UnitArg,

    /// Comma-separated item names. Empty = the F2 default core set
    /// for this statement.
    #[arg(long, value_delimiter = ',')]
    pub items: Vec<String>,

    /// Force a specific upstream source. Default `auto` races every
    /// applicable source and returns the first success — fast but
    /// non-deterministic. Pin `eastmoney` or `sina` when comparing
    /// upstreams or reproducing bugs.
    #[arg(long, value_enum, default_value_t = SourceArg::Auto)]
    pub source: SourceArg,
}

#[derive(Args, Debug)]
pub struct PeriodsArgs {
    /// Single symbol (e.g. `600519` / `00700`).
    pub symbol: String,

    /// Force a specific upstream source. Default `auto` lists periods
    /// from every applicable source (union); pin to one when testing.
    #[arg(long, value_enum, default_value_t = SourceArg::Auto)]
    pub source: SourceArg,
}

/// Which upstream to dispatch to. `Auto` keeps the production
/// first-success-wins behavior; the named variants pin to one source
/// (useful for testing, benchmarking, and isolating upstream bugs).
///
/// Variant names mirror [`SourceTag`] (camelCase); `#[value(name = …)]`
/// pins the CLI surface to the same lower-case label the registered
/// source uses (`--source eastmoney` / `--source sina`).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceArg {
    Auto,
    #[value(name = "eastmoney")]
    EastMoney,
    Sina,
}

impl SourceArg {
    /// Map the CLI choice onto a registered source tag, or `None` for
    /// `Auto` (first-success-wins across every applicable source).
    fn as_source_tag(self) -> Option<SourceTag> {
        match self {
            SourceArg::Auto => None,
            SourceArg::EastMoney => Some(SourceTag::EastMoney),
            SourceArg::Sina => Some(SourceTag::Sina),
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum ScopeArg {
    Consolidated,
    Parent,
}

impl From<ScopeArg> for Scope {
    fn from(v: ScopeArg) -> Self {
        match v {
            ScopeArg::Consolidated => Scope::Consolidated,
            ScopeArg::Parent => Scope::Parent,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum UnitArg {
    Raw,
    Wan,
    Yi,
}

impl From<UnitArg> for Unit {
    fn from(v: UnitArg) -> Self {
        match v {
            UnitArg::Raw => Unit::Raw,
            UnitArg::Wan => Unit::Wan,
            UnitArg::Yi => Unit::Yi,
        }
    }
}

/// Parse one period token. Handles `2024` (bare year) by expanding
/// to all four standard ends.
fn parse_period_token(s: &str) -> Result<Vec<Period>, String> {
    let s = s.trim();
    if s.len() == 4 && s.chars().all(|c| c.is_ascii_digit()) {
        let y: i32 = s.parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
        return Ok(vec![Period::Q1(y), Period::H1(y), Period::Q3(y), Period::Annual(y)]);
    }
    Period::parse(s).map(|p| vec![p]).map_err(|e| e.to_string())
}

/// Entry: dispatch the user's choice.
pub fn run(cmd: ReportCmd, ctx: &ReportContext, fmt: Format) -> Result<(), SiftError> {
    match cmd {
        ReportCmd::Income(a) => run_statement(Statement::Income, a, ctx, fmt),
        ReportCmd::Balance(a) => run_statement(Statement::Balance, a, ctx, fmt),
        ReportCmd::Cashflow(a) => run_statement(Statement::Cashflow, a, ctx, fmt),
        ReportCmd::Indicator(a) => run_statement(Statement::Indicator, a, ctx, fmt),
        ReportCmd::Periods(a) => run_periods(a, ctx, fmt),
    }
}

fn run_statement(
    stmt: Statement,
    args: StatementArgs,
    ctx: &ReportContext,
    fmt: Format,
) -> Result<(), SiftError> {
    let symbols = parse_symbols(&args.symbols)?;
    let scope: Scope = args.scope.into();
    let unit: Unit = args.unit.into();

    validate_scope(scope, &symbols)?;

    let source_name = args.source.as_source_tag().map(SourceTag::as_str);
    let mut all_rows = Vec::new();
    for sym in &symbols {
        let periods = resolve_periods(&args, stmt)?;
        let query = Query {
            symbol: sym.clone(),
            statement: stmt,
            periods,
            scope,
        };
        let rows = dispatch_with_cache_named(&query, ctx, source_name)?;
        let rows = financial_render::apply_unit(rows, unit);
        all_rows.extend(rows);
    }

    // Back-fill the security short name (sina lrb does not return it)
    // from the F1 cninfo listing cache. Empty map if the cache is
    // missing — render falls back to whatever `name` the source
    // already populated.
    let names = load_listing_names(ctx.app);

    let keep = resolve_items(&args.items, stmt);
    let table = pivot(all_rows, keep.as_deref(), &names);
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    financial_render::render(&mut handle, &table, fmt)?;
    emit_unmapped_hint();
    Ok(())
}

fn parse_symbols(raw: &[String]) -> Result<Vec<Symbol>, SiftError> {
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        out.push(Symbol::parse(s)?);
    }
    Ok(out)
}

/// Per the README, `--scope parent` is only valid for A-share. We
/// fail fast here rather than wait for the dispatcher to return
/// `NoApplicableSource`.
fn validate_scope(scope: Scope, symbols: &[Symbol]) -> Result<(), SiftError> {
    if scope == Scope::Parent {
        if let Some(bad) = symbols.iter().find(|s| s.market != Market::CnA) {
            return Err(SiftError::NoApplicableSource(format!(
                "--scope parent is only supported for A-share; got {} ({:?})",
                bad.code, bad.market
            )));
        }
    }
    Ok(())
}

/// Resolve which periods to query given the user's arg combinations.
///
/// Precedence: `--period` > `--last` > `--start/--end + --annual` >
/// default = most recent 8 periods (anchored on the calendar via
/// [`crate::domain::period::most_recent_filed`]).
fn resolve_periods(args: &StatementArgs, _stmt: Statement) -> Result<Vec<Period>, SiftError> {
    if !args.period.is_empty() {
        return Ok(args.period.iter().flat_map(|v| v.iter().copied()).collect());
    }
    let today = time::OffsetDateTime::now_utc().date();
    if let Some(n) = args.last {
        return Ok(last_n_filed(today, n));
    }
    if let (Some(s), Some(e)) = (args.start, args.end) {
        let mut out = Vec::new();
        for y in s..=e {
            if args.annual {
                out.push(Period::Annual(y));
            } else {
                out.push(Period::Q1(y));
                out.push(Period::H1(y));
                out.push(Period::Q3(y));
                out.push(Period::Annual(y));
            }
        }
        return Ok(out);
    }
    // Bare `sift report income <symbol>` (no time arg) — show
    // the most recent 8 periods by default. Dense enough to spot
    // trends without paging.
    Ok(last_n_filed(today, 8))
}

/// Resolve the `--items` filter into a list of standardized item
/// names. Empty input → `None` (no filter: show every column the
/// source returns, in source order). `--items all` is also `None`.
/// Each user-supplied token is normalized through the dictionary so
/// the user can write either the English column name
/// (`PARENT_NETPROFIT`) or a sina-style synonym.
fn resolve_items(raw: &[String], _stmt: Statement) -> Option<Vec<String>> {
    if raw.is_empty() {
        // Default: no filter — render every observed item.
        return None;
    }
    if raw.len() == 1 && raw[0].eq_ignore_ascii_case("all") {
        return None;
    }
    let dict = items_dict::dict();
    let normalized: Vec<String> = raw
        .iter()
        .map(|t| {
            let trimmed = t.trim();
            dict.lookup(trimmed)
                .map(|e| e.cn.clone())
                .unwrap_or_else(|| trimmed.to_string())
        })
        .collect();
    Some(normalized)
}

/// Print `[hint]` line listing items the dictionary did not know
/// about. Drains the collector — calling twice in a row only emits
/// once. No-op when the set is empty.
fn emit_unmapped_hint() {
    let unmapped = items_dict::drain_unmapped();
    if unmapped.is_empty() {
        return;
    }
    let preview: Vec<&str> = unmapped.iter().take(8).map(String::as_str).collect();
    let more = if unmapped.len() > 8 {
        format!(", +{} more", unmapped.len() - 8)
    } else {
        String::new()
    };
    eprintln!(
        "[hint] {} 个科目未在字典中：{}{}",
        unmapped.len(),
        preview.join(", "),
        more
    );
    eprintln!(
        "       帮我们补字典：https://github.com/eavae/sift-cli/issues (TODO once repo public)"
    );
}

// ---------------------------------------------------------------------------
// `periods` subcommand
// ---------------------------------------------------------------------------

fn run_periods(args: PeriodsArgs, ctx: &ReportContext, _fmt: Format) -> Result<(), SiftError> {
    let sym = Symbol::parse(&args.symbol)?;
    let pinned = args.source.as_source_tag().map(SourceTag::as_str);
    let items = list_periods_union(&sym, ctx, pinned)?;
    // Render: period_end / period_type / source — one line per
    // (date, source) pair, newest first (sort is done by the fetch
    // helper).
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    use std::io::Write;
    writeln!(handle, "period\tperiod_type\tsource").ok();
    for (date, pt, name) in items {
        writeln!(
            handle,
            "{}\t{}\t{}",
            date.format(&time::format_description::well_known::Iso8601::DATE)
                .unwrap_or_default(),
            pt.as_str(),
            name,
        )
        .ok();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_period_token_handles_bare_year_and_iso_and_suffix() {
        let four = parse_period_token("2024").unwrap();
        assert_eq!(four.len(), 4);
        assert_eq!(four[0], Period::Q1(2024));
        assert_eq!(four[3], Period::Annual(2024));

        let one = parse_period_token("2024A").unwrap();
        assert_eq!(one, vec![Period::Annual(2024)]);

        let iso = parse_period_token("2024-12-31").unwrap();
        assert_eq!(iso, vec![Period::Annual(2024)]);
    }

    #[test]
    fn resolve_items_empty_means_no_filter() {
        // New default: empty `--items` → render everything the source
        // returned (None), not the README "default 7" core set.
        let items = resolve_items(&[], Statement::Income);
        assert!(items.is_none());
    }

    #[test]
    fn resolve_items_all_returns_no_filter() {
        let items = resolve_items(&["all".to_string()], Statement::Income);
        assert!(items.is_none());
    }

    #[test]
    fn resolve_items_normalizes_english_and_sina_synonyms() {
        let raw = vec![
            "PARENT_NETPROFIT".to_string(),
            "营业总收入".to_string(),
            "归属于母公司股东的净利润".to_string(),
        ];
        let items = resolve_items(&raw, Statement::Income).unwrap();
        assert_eq!(items[0], "归母净利润");
        assert_eq!(items[1], "营业总收入");
        assert_eq!(items[2], "归母净利润");
    }

    #[test]
    fn validate_scope_rejects_parent_for_hk() {
        let hk = vec![Symbol {
            code: "00700".into(),
            market: Market::Hk,
        }];
        let err = validate_scope(Scope::Parent, &hk).unwrap_err();
        assert!(matches!(err, SiftError::NoApplicableSource(_)));
    }

    #[test]
    fn validate_scope_allows_parent_for_a_share() {
        let cn = vec![Symbol {
            code: "600519".into(),
            market: Market::CnA,
        }];
        validate_scope(Scope::Parent, &cn).unwrap();
    }

    // Calendar tests (most_recent_filed, previous, last_n_filed) live
    // in `domain::period::tests` now. This module keeps coverage of
    // its own CLI glue.

    #[test]
    fn source_arg_maps_to_registered_tags() {
        assert_eq!(SourceArg::Auto.as_source_tag(), None);
        assert_eq!(
            SourceArg::EastMoney.as_source_tag(),
            Some(SourceTag::EastMoney)
        );
        assert_eq!(SourceArg::Sina.as_source_tag(), Some(SourceTag::Sina));
    }

    #[test]
    fn source_arg_via_tag_round_trips_to_cli_name() {
        // `--source eastmoney` ↔ SourceArg::EastMoney ↔ SourceTag::EastMoney
        // and the `as_str` label registered by the source's `name()`.
        let cli = SourceArg::EastMoney;
        let tag = cli.as_source_tag().unwrap();
        assert_eq!(tag.as_str(), "eastmoney");
    }
}
