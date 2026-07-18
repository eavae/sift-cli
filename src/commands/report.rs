//! `sift report {income | balance | cashflow | indicator | periods}`.

use clap::{Args, Subcommand, ValueEnum};

use crate::domain::market::{InstrumentKind, Market};
use crate::domain::period::last_n_filed;
use crate::domain::{single_quarter, Period, Query, Scope, SourceTag, Statement, Symbol, Unit};
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

    /// Most-recent N years, anchored on the latest filed period.
    /// `--years 10 --annual` = last 10 annual reports; without
    /// `--annual`, N years of Q1/H1/Q3/Annual. Mutually exclusive
    /// with `--period` / `--last` / `--start` / `--end`.
    #[arg(
        long,
        value_parser = parse_positive_usize,
        conflicts_with_all = ["period", "last", "start", "end"]
    )]
    pub years: Option<usize>,

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

    /// Quarter reporting basis. `cumulative` (default) keeps upstream's
    /// YTD figures (中报=上半年累计 …); `single` converts income /
    /// cashflow to single-quarter values (Q2=H1−Q1 …) for quarter-over-
    /// quarter / seasonality analysis. `single` is flow-statement only.
    #[arg(long, value_enum, default_value_t = QModeArg::Cumulative)]
    pub qmode: QModeArg,

    /// Bypass the per-period cache and refetch from upstream (fresh
    /// rows are still written back). Use when a historical report was
    /// restated — the permanent TTL bucket would otherwise keep
    /// serving the old value.
    #[arg(long)]
    pub no_cache: bool,

    /// Do not write the fetched rows into the local fact store.
    /// By default `report` ingests raw values best-effort (a failure
    /// only warns and never blocks the printed report).
    #[arg(long)]
    pub no_ingest: bool,
}

/// Quarter reporting basis for `--qmode`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum QModeArg {
    /// Upstream YTD-cumulative figures (unchanged).
    Cumulative,
    /// Single-quarter (累计相邻相减); income / cashflow only.
    Single,
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

    let qmode = args.qmode;
    validate_scope(scope, &symbols)?;
    validate_not_index(&symbols)?;
    validate_indicator_market(stmt, &symbols)?;
    validate_qmode(qmode, stmt)?;

    // In single-quarter mode we must fetch every year's four cumulative
    // periods (the raw material for the diff), regardless of what the
    // user asked for — `derive` needs the adjacent cumulative.
    let requested = resolve_periods(&args, stmt)?;
    let fetch_periods = if qmode == QModeArg::Single {
        full_year_cumulative(&requested)
    } else {
        requested
    };

    let source_name = args.source.as_source_tag().map(SourceTag::as_str);
    let mut all_rows = Vec::new();
    for sym in &symbols {
        let query = Query {
            symbol: sym.clone(),
            statement: stmt,
            periods: fetch_periods.clone(),
            scope,
        };
        let rows = dispatch_with_cache_named(&query, ctx, source_name, args.no_cache)?;
        all_rows.extend(rows);
    }

    // 累计 → 单季 conversion runs on raw values, before unit scaling.
    if qmode == QModeArg::Single {
        let (singles, missing) = single_quarter::derive(all_rows);
        warn_missing_single_quarters(&missing);
        all_rows = singles;
    }

    // Ingest raw rows into the fact store (best-effort) *before*
    // `apply_unit` — the store holds unscaled values. Failure never
    // blocks the printed report.
    if !args.no_ingest {
        match crate::service::facts::ingest_statement(ctx.app, &all_rows, qmode == QModeArg::Single)
        {
            Ok(Some(_)) => {}
            Ok(None) => eprintln!("[warn] 事实库不可用，跳过入库（不影响输出）"),
            Err(e) => eprintln!("[warn] facts 入库失败（不影响输出）: {e}"),
        }
    }

    let all_rows = financial_render::apply_unit(all_rows, unit);

    // Back-fill the security short name (sina lrb does not return it)
    // from the cninfo listing cache. Empty map if the cache is
    // missing — render falls back to whatever `name` the source
    // already populated.
    let names = load_listing_names(ctx.app);

    let keep = resolve_items(&args.items, stmt);
    let mut table = pivot(all_rows, keep.as_deref(), &names);
    table.single_quarter = qmode == QModeArg::Single;
    warn_unmatched_items(keep.as_deref(), &table);
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    financial_render::render(&mut handle, &table, fmt)?;
    Ok(())
}

/// Expand a period list to every touched year's four cumulative ends
/// (Q1/H1/Q3/Annual) — the fetch set single mode derives singles from.
fn full_year_cumulative(periods: &[Period]) -> Vec<Period> {
    let mut years: Vec<i32> = periods.iter().map(|p| p.end_date().year()).collect();
    years.sort_unstable();
    years.dedup();
    let mut out = Vec::with_capacity(years.len() * 4);
    for y in years {
        out.push(Period::Q1(y));
        out.push(Period::H1(y));
        out.push(Period::Q3(y));
        out.push(Period::Annual(y));
    }
    out
}

/// `--qmode single` only makes sense for the flow statements. Balance
/// is a point-in-time snapshot; indicator is ratios — neither has a
/// cumulative→single subtraction.
fn validate_qmode(qmode: QModeArg, stmt: Statement) -> Result<(), SiftError> {
    if qmode == QModeArg::Single && !matches!(stmt, Statement::Income | Statement::Cashflow) {
        return Err(SiftError::NoApplicableSource(format!(
            "--qmode single 仅适用于 income / cashflow（{} 是时点数 / 比率，无单季口径）",
            stmt.as_str()
        )));
    }
    Ok(())
}

/// One `[warn]` summarizing single quarters that couldn't be derived
/// because an intermediate cumulative period was missing (e.g. the
/// annual is not yet filed, so Q4 is unavailable).
fn warn_missing_single_quarters(missing: &[single_quarter::Missing]) {
    if missing.is_empty() {
        return;
    }
    let preview: Vec<String> = missing
        .iter()
        .take(6)
        .map(|m| format!("{} {}Q{}", m.item, m.year, m.quarter))
        .collect();
    let more = if missing.len() > 6 {
        format!(", +{} more", missing.len() - 6)
    } else {
        String::new()
    };
    eprintln!(
        "[warn] {} 个单季因缺中间累计期无法换算（如年报未披露 → Q4）: {}{}",
        missing.len(),
        preview.join(", "),
        more
    );
}

/// Print a one-line `[warn]` for each `--items` entry that matched no
/// column in the pivoted output — `--items` is an exact-match filter,
/// so a typo / wrong alias otherwise degrades to a silently empty
/// table with no feedback.
fn warn_unmatched_items(keep: Option<&[String]>, table: &financial_render::PivotedTable) {
    let Some(filter) = keep else { return };
    let missing = unmatched_items(filter, table);
    if !missing.is_empty() {
        eprintln!(
            "[warn] --items 中有 {} 个科目未匹配（精确匹配；不带 --items 运行可先看全部列名）: {}",
            missing.len(),
            missing.join(", ")
        );
    }
}

/// Pure core of [`warn_unmatched_items`]: the `--items` tokens that
/// matched no column in the pivoted output, in input order.
fn unmatched_items<'a>(
    filter: &'a [String],
    table: &financial_render::PivotedTable,
) -> Vec<&'a str> {
    let present: std::collections::HashSet<&str> = table
        .blocks
        .iter()
        .flat_map(|b| b.items.iter().map(|(n, _)| n.as_str()))
        .collect();
    filter
        .iter()
        .map(String::as_str)
        .filter(|i| !present.contains(i))
        .collect()
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
/// Precedence: `--period` > `--last` > `--years` >
/// `--start/--end + --annual` > default = most recent 8 periods
/// (anchored on the calendar via
/// [`crate::domain::period::most_recent_filed`]).
fn resolve_periods(args: &StatementArgs, _stmt: Statement) -> Result<Vec<Period>, SiftError> {
    if !args.period.is_empty() {
        return Ok(args.period.iter().flat_map(|v| v.iter().copied()).collect());
    }
    let today = time::OffsetDateTime::now_utc().date();
    if let Some(n) = args.last {
        return Ok(last_n_filed(today, n));
    }
    if let Some(n) = args.years {
        let anchor = anchor_year(today, args.annual);
        return Ok(expand_year_range(anchor - (n as i32) + 1, anchor, args.annual));
    }
    if let (Some(s), Some(e)) = (args.start, args.end) {
        return Ok(expand_year_range(s, e, args.annual));
    }
    // Bare `sift report income <symbol>` (no time arg) — show
    // the most recent 8 periods by default. Dense enough to spot
    // trends without paging.
    Ok(last_n_filed(today, 8))
}

/// Expand an inclusive year range into periods: one `Annual(y)` per
/// year when `annual`, else the four standard ends per year. Shared by
/// the `--start/--end` and `--years` branches of [`resolve_periods`].
fn expand_year_range(start: i32, end: i32, annual: bool) -> Vec<Period> {
    let mut out = Vec::new();
    for y in start..=end {
        if annual {
            out.push(Period::Annual(y));
        } else {
            out.push(Period::Q1(y));
            out.push(Period::H1(y));
            out.push(Period::Q3(y));
            out.push(Period::Annual(y));
        }
    }
    out
}

/// The most-recent year to anchor `--years N` on. `annual` walks back
/// from [`most_recent_filed`] to the latest year whose Annual is
/// guaranteed filed (so `--years N --annual` never yields an empty
/// trailing column); otherwise uses the latest filed period's year.
fn anchor_year(today: time::Date, annual: bool) -> i32 {
    let mut p = crate::domain::period::most_recent_filed(today);
    if annual {
        while p.period_type() != Some(crate::domain::PeriodType::Annual) {
            p = p.previous();
        }
    }
    p.end_date().year()
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
    fn expand_year_range_annual_vs_quarterly() {
        assert_eq!(
            expand_year_range(2023, 2025, true),
            vec![Period::Annual(2023), Period::Annual(2024), Period::Annual(2025)]
        );
        let q = expand_year_range(2024, 2024, false);
        assert_eq!(
            q,
            vec![
                Period::Q1(2024),
                Period::H1(2024),
                Period::Q3(2024),
                Period::Annual(2024)
            ]
        );
    }

    #[test]
    fn anchor_year_annual_backs_off_to_latest_filed_annual() {
        use time::Month;
        let d = |y: i32, m: u8, day: u8| {
            time::Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap()
        };
        // May 2026: most_recent_filed = Q1(2026); annual anchor walks
        // back to Annual(2025).
        assert_eq!(anchor_year(d(2026, 5, 20), true), 2025);
        assert_eq!(anchor_year(d(2026, 5, 20), false), 2026);
        // Feb 2026 (Jan–Apr): most_recent_filed = Q3(2025) → annual
        // anchor Annual(2024) (Annual(2025) not guaranteed filed).
        assert_eq!(anchor_year(d(2026, 2, 10), true), 2024);
    }

    #[test]
    fn years_conflicts_with_other_period_selectors_at_clap_level() {
        use clap::Parser;
        for other in [["--period", "2024A"], ["--last", "4"], ["--start", "2020"]] {
            let res = crate::cli::Cli::try_parse_from(
                ["sift", "report", "income", "600519", "--years", "3"]
                    .into_iter()
                    .chain(other),
            );
            let err = res.unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "expected conflict with {other:?}"
            );
        }
    }

    #[test]
    fn years_zero_rejected_at_clap_level() {
        use clap::Parser;
        let res = crate::cli::Cli::try_parse_from([
            "sift", "report", "income", "600519", "--years", "0",
        ]);
        assert!(res.is_err());
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
    fn unmatched_items_flags_only_absent_tokens() {
        use crate::output::financial_render::{PivotedTable, SymbolBlock};
        // A block that presents exactly two raw EM columns.
        let block = SymbolBlock {
            symbol: Symbol {
                code: "600519".into(),
                market: Market::CnA,
                kind: InstrumentKind::Equity,
            },
            name: "贵州茅台".into(),
            statement: Statement::Income,
            scope: "consolidated".into(),
            currency: "CNY".into(),
            unit: Unit::Raw,
            periods: vec![],
            items: vec![
                ("TOTAL_OPERATE_INCOME".into(), vec![]),
                ("BASIC_EPS".into(), vec![]),
            ],
            sources: vec![],
        };
        let table = PivotedTable {
            blocks: vec![block],
            single_quarter: false,
        };

        let filter = vec![
            "TOTAL_OPERATE_INCOME".to_string(), // present
            "ROE".to_string(),                  // absent (typo / wrong label)
            "BASIC_EPS".to_string(),            // present
        ];
        let missing = unmatched_items(&filter, &table);
        assert_eq!(missing, vec!["ROE"]);

        // All-present → nothing flagged.
        let ok = vec!["BASIC_EPS".to_string()];
        assert!(unmatched_items(&ok, &table).is_empty());
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
    fn validate_qmode_single_flow_only() {
        // single OK for income / cashflow.
        validate_qmode(QModeArg::Single, Statement::Income).unwrap();
        validate_qmode(QModeArg::Single, Statement::Cashflow).unwrap();
        // rejected for balance (point-in-time) / indicator (ratios).
        for stmt in [Statement::Balance, Statement::Indicator] {
            let err = validate_qmode(QModeArg::Single, stmt).unwrap_err();
            assert!(matches!(err, SiftError::NoApplicableSource(_)));
            assert!(err.to_string().contains("income / cashflow"), "msg: {err}");
        }
        // cumulative is always fine.
        validate_qmode(QModeArg::Cumulative, Statement::Balance).unwrap();
    }

    #[test]
    fn full_year_cumulative_expands_distinct_years_to_four_ends() {
        // Sub-year + duplicate-year input → each distinct year's 4 ends.
        let got = full_year_cumulative(&[
            Period::Q3(2024),
            Period::Annual(2024),
            Period::Q1(2023),
        ]);
        assert_eq!(
            got,
            vec![
                Period::Q1(2023),
                Period::H1(2023),
                Period::Q3(2023),
                Period::Annual(2023),
                Period::Q1(2024),
                Period::H1(2024),
                Period::Q3(2024),
                Period::Annual(2024),
            ]
        );
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
