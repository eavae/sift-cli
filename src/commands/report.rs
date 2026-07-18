//! `sift report {income | balance | cashflow | indicator | periods}`.

use clap::{Args, Subcommand, ValueEnum};

use crate::domain::market::{InstrumentKind, Market};
use crate::domain::period::last_n_filed;
use crate::domain::{Period, Query, Scope, SourceTag, Statement, Symbol, Unit};
use crate::error::SiftError;
use crate::fetch::report::{
    dispatch_with_cache_named, list_periods_union, load_listing_names, ReportContext,
};
use crate::output::financial_render::{self, pivot};
use crate::output::Format;

/// CLI surface for `sift report`.
#[derive(Subcommand, Debug)]
pub enum ReportCmd {
    #[command(
        about = "Income statement (营业总收入 / 净利润 / 毛利率 …)",
        after_long_help = "Examples:\n  \
                           sift report income 600519 --last 4 --unit yi\n  \
                           sift report income 600519 600036 --period 2024A,2023A --scope parent --format tsv\n  \
                           sift report income 600519 --start 2020 --end 2024 --annual --items 营业总收入,净利润\n  \
                           sift report income 00700 --period 2023A                          # HK: omit --items (different vocab)\n  \
                           sift report income 600519 --last 8 --source eastmoney            # pin upstream for repro"
    )]
    Income(StatementArgs),
    #[command(
        about = "Balance sheet (资产总计 / 负债合计 / 股东权益 …)",
        after_long_help = "Examples:\n  \
                           sift report balance 600519 --last 4 --unit yi\n  \
                           sift report balance 600519 600036 --period 2024A --scope parent --format tsv\n  \
                           sift report balance 600519 --start 2020 --end 2024 --annual --items 资产总计,负债合计\n  \
                           sift report balance 00700 --period 2023A                         # HK: omit --items (different vocab)\n  \
                           sift report balance 600519 --period 2024Q3 --source sina         # pin upstream"
    )]
    Balance(StatementArgs),
    #[command(
        about = "Cashflow statement (经营 / 投资 / 筹资活动现金流量 …)",
        after_long_help = "Examples:\n  \
                           sift report cashflow 600519 --last 4 --unit yi\n  \
                           sift report cashflow 600519 --start 2020 --end 2024 --annual --format tsv\n  \
                           sift report cashflow 600519 600036 --period 2024A --scope parent\n  \
                           sift report cashflow 600519 --period 2024H1 --items 经营活动产生的现金流量净额"
    )]
    Cashflow(StatementArgs),
    #[command(
        about = "Key financial indicators (ROE / EPS / 毛利率 / 资产负债率 …)",
        after_long_help = "Examples:\n  \
                           sift report indicator 600519 --last 8\n  \
                           sift report indicator 600519 --start 2020 --end 2024 --annual --items ROE加权,EPS,毛利率\n  \
                           sift report indicator 600519 600036 --period 2024A --format tsv\n  \
                           sift report indicator 600519 --last 4 --source eastmoney\n\n\
                           Note: HK indicator is not yet implemented — for 00700 et al, use `report income/balance/cashflow` instead."
    )]
    Indicator(StatementArgs),
    #[command(
        about = "List the report periods available upstream for a symbol",
        after_long_help = "Examples:\n  \
                           sift report periods 600519                          # A-share via EM income-date list\n  \
                           sift report periods 00700                           # HK via EM income long-table\n  \
                           sift report periods 600519 --source eastmoney       # explicit pin\n  \
                           sift report periods 600519 --format tsv | awk -F'\\t' '!/^#/ {print $1}'   # bare period list"
    )]
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
    #[arg(long, value_parser = parse_positive_usize, conflicts_with = "period")]
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

    /// Comma-separated item names. Empty = the default core set
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

/// clap value parser for `--last`: rejects 0 (which would silently
/// query nothing and exit 0 with an empty table) the same way clap's
/// other value rejections do — exit code 2 with an inline message.
fn parse_positive_usize(s: &str) -> Result<usize, String> {
    let n: usize = s
        .parse()
        .map_err(|e: std::num::ParseIntError| format!("invalid value {s:?}: {e}"))?;
    if n == 0 {
        return Err("value must be at least 1 (got 0)".into());
    }
    Ok(n)
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
    validate_not_index(&symbols)?;
    validate_indicator_market(stmt, &symbols)?;

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
    // from the cninfo listing cache. Empty map if the cache is
    // missing — render falls back to whatever `name` the source
    // already populated.
    let names = load_listing_names(ctx.app);

    let keep = resolve_items(&args.items, stmt);
    let table = pivot(all_rows, keep.as_deref(), &names);
    warn_unmatched_items(keep.as_deref(), &table);
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    financial_render::render(&mut handle, &table, fmt)?;
    Ok(())
}

/// Print a one-line `[warn]` for each `--items` entry that matched no
/// column in the pivoted output — `--items` is an exact-match filter,
/// so a typo / wrong alias otherwise degrades to a silently empty
/// table with no feedback.
fn warn_unmatched_items(keep: Option<&[String]>, table: &financial_render::PivotedTable) {
    let Some(filter) = keep else { return };
    let present: std::collections::HashSet<&str> = table
        .blocks
        .iter()
        .flat_map(|b| b.items.iter().map(|(n, _)| n.as_str()))
        .collect();
    let missing: Vec<&str> = filter
        .iter()
        .map(String::as_str)
        .filter(|i| !present.contains(i))
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "[warn] --items 中有 {} 个科目未匹配（精确匹配；不带 --items 运行可先看全部列名）: {}",
            missing.len(),
            missing.join(", ")
        );
    }
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

/// Indices are quote/bars-only instruments — no fundamentals upstream
/// carries them. Fail fast with a pointer to the right command instead
/// of surfacing a confusing per-source "unsupported" error.
fn validate_not_index(symbols: &[Symbol]) -> Result<(), SiftError> {
    if let Some(bad) = symbols.iter().find(|s| s.kind == InstrumentKind::Index) {
        return Err(SiftError::NoApplicableSource(format!(
            "index {} (`sift report` covers equities only; use `sift quote` / `sift bars`)",
            bad.display_symbol()
        )));
    }
    Ok(())
}

/// `report indicator` is only implemented for A-share — the HK
/// (`RPT_HKF10_FN_MAININDICATOR`) and US indicator adapters are
/// stubbed. Fail fast with a clear pointer instead of returning a
/// silently empty table (the source `supports` matrix would otherwise
/// admit HK consolidated and the stub would hand back zero rows).
fn validate_indicator_market(stmt: Statement, symbols: &[Symbol]) -> Result<(), SiftError> {
    if stmt != Statement::Indicator {
        return Ok(());
    }
    if let Some(bad) = symbols.iter().find(|s| s.market != Market::CnA) {
        return Err(SiftError::NoApplicableSource(format!(
            "indicator is A-share only ({} is {}); use `report income/balance/cashflow` instead",
            bad.display_symbol(),
            bad.market.as_upper(),
        )));
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

/// Resolve the `--items` filter into a list of item labels to keep.
/// Empty input → `None` (no filter: show every column the source
/// returns, in source order). `--items all` is also `None`.
/// Tokens match the **raw upstream label** exactly (trimmed) — there
/// is no name dictionary, so for A-share EM the user writes the
/// English column code (`PARENT_NETPROFIT`), and for HK / sina the
/// native Chinese label. `warn_unmatched_items` flags any token that
/// matched no column in the result.
fn resolve_items(raw: &[String], _stmt: Statement) -> Option<Vec<String>> {
    if raw.is_empty() {
        // Default: no filter — render every observed item.
        return None;
    }
    if raw.len() == 1 && raw[0].eq_ignore_ascii_case("all") {
        return None;
    }
    Some(raw.iter().map(|t| t.trim().to_string()).collect())
}

// ---------------------------------------------------------------------------
// `periods` subcommand
// ---------------------------------------------------------------------------

fn run_periods(args: PeriodsArgs, ctx: &ReportContext, fmt: Format) -> Result<(), SiftError> {
    let sym = Symbol::parse(&args.symbol)?;
    validate_not_index(std::slice::from_ref(&sym))?;
    let pinned = args.source.as_source_tag().map(SourceTag::as_str);
    let items = list_periods_union(&sym, ctx, pinned)?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    use std::io::Write;
    // Header: `#`-prefixed in TSV so `awk '!/^#/'` / pandas `comment='#'`
    // skip it; bare in the human Table view.
    let header = if matches!(fmt, Format::Tsv) {
        "#period\tperiod_type\tsource"
    } else {
        "period\tperiod_type\tsource"
    };
    writeln!(handle, "{header}").ok();
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
    fn parse_positive_usize_rejects_zero_and_garbage() {
        assert_eq!(parse_positive_usize("4").unwrap(), 4);
        let err = parse_positive_usize("0").unwrap_err();
        assert!(err.contains("at least 1"), "err: {err}");
        assert!(parse_positive_usize("abc").is_err());
    }

    #[test]
    fn last_conflicts_with_period_at_clap_level() {
        use clap::Parser;
        let res = crate::cli::Cli::try_parse_from([
            "sift", "report", "income", "600519", "--last", "2", "--period", "2024Q3",
        ]);
        // clap usage error → exit code 2 (matches other arg conflicts).
        let err = res.unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

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
    fn resolve_items_keeps_raw_tokens_trimmed() {
        // No dictionary: tokens are the raw upstream labels the user
        // must match exactly (EM English codes / native Chinese),
        // only trimmed of surrounding whitespace.
        let raw = vec![
            "PARENT_NETPROFIT".to_string(),
            "  TOTAL_OPERATE_INCOME  ".to_string(),
            "归属于母公司股东的净利润".to_string(),
        ];
        let items = resolve_items(&raw, Statement::Income).unwrap();
        assert_eq!(items[0], "PARENT_NETPROFIT");
        assert_eq!(items[1], "TOTAL_OPERATE_INCOME");
        assert_eq!(items[2], "归属于母公司股东的净利润");
    }

    #[test]
    fn validate_not_index_rejects_index_symbols() {
        let idx = vec![Symbol {
            code: "000001".into(),
            market: Market::CnA,
            kind: InstrumentKind::Index,
        }];
        let err = validate_not_index(&idx).unwrap_err();
        assert!(matches!(err, SiftError::NoApplicableSource(_)));
        assert!(err.to_string().contains("sh000001"), "msg: {err}");

        let eq = vec![Symbol {
            code: "600519".into(),
            market: Market::CnA,
            kind: InstrumentKind::Equity,
        }];
        validate_not_index(&eq).unwrap();
    }

    #[test]
    fn validate_indicator_market_rejects_hk_but_allows_a_share() {
        let hk = vec![Symbol {
            code: "00700".into(),
            market: Market::Hk,
            kind: InstrumentKind::Equity,
        }];
        // Indicator on HK → clear error (was a silent empty table).
        let err = validate_indicator_market(Statement::Indicator, &hk).unwrap_err();
        assert!(matches!(err, SiftError::NoApplicableSource(_)));
        assert!(err.to_string().contains("income/balance/cashflow"), "msg: {err}");

        // Non-indicator statements on HK are unaffected.
        validate_indicator_market(Statement::Income, &hk).unwrap();

        // Indicator on A-share is fine.
        let cn = vec![Symbol {
            code: "600519".into(),
            market: Market::CnA,
            kind: InstrumentKind::Equity,
        }];
        validate_indicator_market(Statement::Indicator, &cn).unwrap();
    }

    #[test]
    fn validate_scope_rejects_parent_for_hk() {
        let hk = vec![Symbol {
            code: "00700".into(),
            market: Market::Hk,
            kind: crate::domain::market::InstrumentKind::Equity,
        }];
        let err = validate_scope(Scope::Parent, &hk).unwrap_err();
        assert!(matches!(err, SiftError::NoApplicableSource(_)));
    }

    #[test]
    fn validate_scope_allows_parent_for_a_share() {
        let cn = vec![Symbol {
            code: "600519".into(),
            market: Market::CnA,
            kind: crate::domain::market::InstrumentKind::Equity,
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
