//! `sift bars <symbol>...` — historical K-line bars.
//!
//! The command does five things:
//! 1. For each symbol build a [`BarsQuery`] and call
//!    [`fetch::bars::dispatch_named`] — the command does **not**
//!    import `sources::*` directly; source URL building, parsing,
//!    and unit conversion all live behind the
//!    [`crate::sources::bars_source::BarsSource`] trait.
//! 2. If every symbol fails return `SiftError::AllSourcesFailed`
//!    (exit code 3); stdout stays untouched.
//! 3. Otherwise dispatch on `fmt`:
//!    - `Table`: multi-symbol input renders grouped via
//!      `output::bars::render_grouped`; single-symbol input renders
//!      a plain long table.
//!    - `Tsv` / `Json`: always emit the flat long form through
//!      `output::render` (TSV rows / NDJSON objects).
//! 4. After stdout is fully written, drain collected failures to
//!    stderr so warns never interleave with data.

use std::io::Write;

use clap::Args;
use time::format_description::well_known::Iso8601;
use time::{Date, Duration, OffsetDateTime};

use crate::domain::bars::{Adjust, BarRow, BarsQuery, Period};
use crate::domain::Symbol;
use crate::error::SiftError;
use crate::fetch::bars::{dispatch_named, BarsContext};
use crate::output::{self, Format};

#[derive(Args, Debug)]
pub struct BarsArgs {
    /// One or more symbols (6 digits for CN A-share, 5 digits for
    /// HK; forms like `600519` or `00700.HK` are accepted; indices
    /// use an explicit exchange prefix — `sh000001` = 上证指数,
    /// `sz399001` = 深证成指).
    /// Multiple symbols are fetched serially; a per-symbol failure
    /// surfaces as a `[warn]` line on stderr without aborting the
    /// run.
    #[arg(required = true)]
    pub symbols: Vec<String>,

    /// Start date in `YYYY-MM-DD`. Defaults to one year before today.
    #[arg(long)]
    pub start: Option<String>,

    /// End date in `YYYY-MM-DD`. Defaults to today.
    #[arg(long)]
    pub end: Option<String>,

    /// Take the most recent N bars (counted in `--period` units —
    /// so `--period weekly --limit 12` is the last 12 weeks).
    /// Mutually exclusive with `--start` / `--end`.
    #[arg(long, conflicts_with_all = &["start", "end"])]
    pub limit: Option<usize>,

    /// Price adjustment: `pre` (default — dividends / splits folded
    /// back into history, anchored at the latest close), `none` (raw
    /// broker closes), or `post`.
    #[arg(long, value_enum, default_value_t = AdjustArg::Pre)]
    pub adjust: AdjustArg,

    /// Bar granularity: `daily` (default), `weekly`, or `monthly`.
    /// Quarterly / yearly are intentionally unsupported; resample
    /// downstream (e.g. pandas `df.resample('Q')`).
    #[arg(long, value_enum, default_value_t = PeriodArg::Daily)]
    pub period: PeriodArg,

    /// Upstream: `tencent` (default — broader connectivity) or
    /// `eastmoney` (natively reports `amount` / `pct_change` /
    /// `change` / `amplitude_pct`; tencent values are computed
    /// client-side).
    #[arg(long, value_enum, default_value_t = SourceArg::Tencent)]
    pub source: SourceArg,
}

/// `clap::ValueEnum` shadow of [`Adjust`].
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[clap(rename_all = "lowercase")]
pub enum AdjustArg {
    None,
    Pre,
    Post,
}

impl From<AdjustArg> for Adjust {
    fn from(a: AdjustArg) -> Self {
        match a {
            AdjustArg::None => Adjust::None,
            AdjustArg::Pre => Adjust::Pre,
            AdjustArg::Post => Adjust::Post,
        }
    }
}

/// `clap::ValueEnum` shadow of [`Period`].
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[clap(rename_all = "lowercase")]
pub enum PeriodArg {
    Daily,
    Weekly,
    Monthly,
}

impl From<PeriodArg> for Period {
    fn from(p: PeriodArg) -> Self {
        match p {
            PeriodArg::Daily => Period::Daily,
            PeriodArg::Weekly => Period::Weekly,
            PeriodArg::Monthly => Period::Monthly,
        }
    }
}

/// Upstream selector for `sift bars`. The literal lowercases
/// (`tencent` / `eastmoney`) must match the `name()` returned by
/// each [`crate::sources::bars_source::BarsSource`] impl, since
/// [`dispatch_named`] looks up by exact string match.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[clap(rename_all = "lowercase")]
pub enum SourceArg {
    Tencent,
    Eastmoney,
}

impl SourceArg {
    fn as_name(self) -> &'static str {
        match self {
            SourceArg::Tencent => "tencent",
            SourceArg::Eastmoney => "eastmoney",
        }
    }
}

pub fn run(args: BarsArgs, ctx: &BarsContext, fmt: Format) -> Result<(), SiftError> {
    let period: Period = args.period.into();
    let adjust: Adjust = args.adjust.into();
    let (start, end) = resolve_date_range(&args, period)?;
    let source_name = args.source.as_name();

    // Pass 1 — collect only; write nothing to stdout or stderr yet.
    let mut all_rows: Vec<BarRow> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    let symbol_count = args.symbols.len();

    for raw in &args.symbols {
        let res = Symbol::parse(raw).and_then(|sym| {
            let q = BarsQuery {
                symbol: sym,
                start,
                end,
                limit: args.limit,
                adjust,
                period,
            };
            dispatch_named(&q, ctx, source_name)
        });
        match res {
            Ok(rows) => all_rows.extend(rows),
            Err(e) => failures.push((raw.clone(), e.to_string())),
        }
    }

    // All failed: skip stdout entirely; the error is the report.
    if all_rows.is_empty() {
        return Err(SiftError::AllSourcesFailed(
            failures
                .into_iter()
                .map(|(sym, cause)| (format!("bars {sym}"), cause))
                .collect(),
        ));
    }

    // Pass 2 — write all of stdout in one go.
    {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        match fmt {
            Format::Table => {
                if symbol_count == 1 {
                    output::render(&mut handle, fmt, &all_rows)?;
                } else {
                    output::bars::render_grouped(&mut handle, &all_rows)?;
                }
            }
            Format::Tsv | Format::Json => output::render(&mut handle, fmt, &all_rows)?,
        }
    }

    // Pass 3 — partial failure: drain failures to stderr only after
    // stdout is fully flushed.
    if !failures.is_empty() {
        let stderr = std::io::stderr();
        let mut err = stderr.lock();
        for (sym, cause) in &failures {
            let _ = writeln!(err, "[warn] bars {sym}: {cause}");
        }
    }
    Ok(())
}

/// `--limit` is mutually exclusive with the date flags (enforced by
/// clap). When `--limit` is set we leave both bounds at `None` and
/// let the source layer fetch all available rows, then trim to the
/// most recent N client-side. Otherwise the default range is
/// `today - default_lookback_days(period)` … `today`, which scales
/// with the chosen period so the user always sees roughly one
/// screenful of bars by default regardless of granularity.
fn resolve_date_range(
    args: &BarsArgs,
    period: Period,
) -> Result<(Option<Date>, Option<Date>), SiftError> {
    if args.limit.is_some() {
        return Ok((None, None));
    }
    let today = OffsetDateTime::now_utc().date();
    let start = match &args.start {
        Some(s) => Some(parse_iso_date(s)?),
        None => Some(today.saturating_sub(Duration::days(default_lookback_days(period)))),
    };
    let end = match &args.end {
        Some(s) => Some(parse_iso_date(s)?),
        None => Some(today),
    };
    Ok((start, end))
}

/// How far back to default to when neither `--start`/`--end` nor
/// `--limit` is given. Targets ~250 bars per period so daily,
/// weekly, and monthly all show roughly one screenful by default:
///
/// - **daily**: 365 days ≈ 250 trading days (≈ 1 year of A-share).
/// - **weekly**: 5 × 365 ≈ 260 weekly bars (≈ 5 years).
/// - **monthly**: 20 × 365 ≈ 240 monthly bars (≈ 20 years — the
///   typical CN-A listing history is ≤ 30 years so this is a near-
///   full sweep for most issuers).
fn default_lookback_days(period: Period) -> i64 {
    match period {
        Period::Daily => 365,
        Period::Weekly => 365 * 5,
        Period::Monthly => 365 * 20,
    }
}

fn parse_iso_date(s: &str) -> Result<Date, SiftError> {
    Date::parse(s, &Iso8601::DATE).map_err(|e| {
        SiftError::Parse(format!("invalid date {s:?} (expected YYYY-MM-DD): {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppContext;
    use crate::http::HttpClient;
    use crate::sources::bars_source::BarsSource;

    /// Mock source returning a fixed row. Used so command-layer
    /// tests do not need mockito — they exercise the
    /// dispatch + render plumbing, not source HTTP details.
    struct MockSource {
        name: &'static str,
    }
    impl BarsSource for MockSource {
        fn name(&self) -> &'static str {
            self.name
        }
        fn fetch(
            &self,
            q: &BarsQuery,
            _http: &HttpClient,
        ) -> Result<Vec<BarRow>, SiftError> {
            // 999999 = synthetic failure trigger; the all-fail and
            // partial-fail tests rely on this.
            if q.symbol.code.starts_with("999") {
                return Err(SiftError::NotFound(q.symbol.code.clone()));
            }
            Ok(vec![BarRow {
                symbol: format!("{}.{}", q.symbol.code, q.symbol.market.as_upper()),
                date: "2024-01-02".into(),
                open: 10.0,
                high: 20.0,
                low: 8.0,
                close: 15.0,
                volume: 10_000,
                amount: 1000.0,
                pct_change: 0.0,
                change: 0.0,
                amplitude_pct: 0.0,
                adjust: q.adjust,
                period: q.period,
                source: self.name,
            }])
        }
    }

    fn args(symbols: Vec<&str>) -> BarsArgs {
        BarsArgs {
            symbols: symbols.into_iter().map(String::from).collect(),
            start: None,
            end: None,
            limit: Some(1),
            adjust: AdjustArg::Pre,
            period: PeriodArg::Daily,
            source: SourceArg::Tencent,
        }
    }

    #[test]
    fn default_lookback_scales_with_period() {
        assert_eq!(default_lookback_days(Period::Daily), 365);
        assert_eq!(default_lookback_days(Period::Weekly), 365 * 5);
        assert_eq!(default_lookback_days(Period::Monthly), 365 * 20);
    }

    #[test]
    fn resolve_date_range_uses_period_specific_default() {
        let mk = |period: PeriodArg| BarsArgs {
            symbols: vec!["600519".into()],
            start: None,
            end: None,
            limit: None,
            adjust: AdjustArg::Pre,
            period,
            source: SourceArg::Tencent,
        };
        for (period_arg, period) in [
            (PeriodArg::Daily, Period::Daily),
            (PeriodArg::Weekly, Period::Weekly),
            (PeriodArg::Monthly, Period::Monthly),
        ] {
            let (start, end) = resolve_date_range(&mk(period_arg), period).unwrap();
            let s = start.expect("start defaulted");
            let e = end.expect("end defaulted");
            let span = (e - s).whole_days();
            assert_eq!(
                span,
                default_lookback_days(period),
                "{period:?}: span mismatch"
            );
        }
    }

    #[test]
    fn resolve_date_range_returns_none_when_limit_set() {
        let mut a = args(vec!["600519"]);
        a.limit = Some(20);
        let (s, e) = resolve_date_range(&a, Period::Monthly).unwrap();
        assert!(s.is_none());
        assert!(e.is_none());
    }

    #[test]
    fn json_format_is_accepted_and_renders_ndjson() {
        // `--format json` goes through the generic RenderRow → NDJSON
        // pipeline like every other command; run() must not reject it.
        let app = AppContext::default();
        let sources: Vec<Box<dyn BarsSource>> = vec![Box::new(MockSource { name: "tencent" })];
        let ctx = BarsContext {
            app: &app,
            sources: &sources,
        };
        let res = run(args(vec!["600519"]), &ctx, Format::Json);
        assert!(res.is_ok(), "json format should be accepted: {res:?}");
    }

    #[test]
    fn invalid_date_yields_parse_error() {
        let app = AppContext::default();
        let sources: Vec<Box<dyn BarsSource>> = vec![Box::new(MockSource { name: "tencent" })];
        let ctx = BarsContext {
            app: &app,
            sources: &sources,
        };
        let mut a = args(vec!["600519"]);
        a.limit = None;
        a.start = Some("2024-13-01".into());
        let err = run(a, &ctx, Format::Tsv).unwrap_err();
        assert!(matches!(err, SiftError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn all_symbols_failing_yields_all_sources_failed() {
        let app = AppContext::default();
        let sources: Vec<Box<dyn BarsSource>> = vec![Box::new(MockSource { name: "tencent" })];
        let ctx = BarsContext {
            app: &app,
            sources: &sources,
        };
        let err = run(args(vec!["999999", "999998"]), &ctx, Format::Table).unwrap_err();
        match err {
            SiftError::AllSourcesFailed(v) => {
                assert_eq!(v.len(), 2);
                assert!(v[0].0.starts_with("bars "));
            }
            other => panic!("expected AllSourcesFailed, got {other:?}"),
        }
    }

    #[test]
    fn partial_failure_returns_ok_with_successful_rows() {
        let app = AppContext::default();
        let sources: Vec<Box<dyn BarsSource>> = vec![Box::new(MockSource { name: "tencent" })];
        let ctx = BarsContext {
            app: &app,
            sources: &sources,
        };
        let res = run(args(vec!["999999", "600519"]), &ctx, Format::Tsv);
        assert!(res.is_ok(), "partial failure should still exit 0: {res:?}");
    }

    #[test]
    fn unknown_source_yields_no_applicable_source() {
        let app = AppContext::default();
        // Only tencent registered, args ask for eastmoney → dispatch
        // returns NoApplicableSource per symbol; all_rows empty →
        // AllSourcesFailed.
        let sources: Vec<Box<dyn BarsSource>> = vec![Box::new(MockSource { name: "tencent" })];
        let ctx = BarsContext {
            app: &app,
            sources: &sources,
        };
        let mut a = args(vec!["600519"]);
        a.source = SourceArg::Eastmoney;
        let err = run(a, &ctx, Format::Tsv).unwrap_err();
        match err {
            SiftError::AllSourcesFailed(v) => {
                assert!(v[0].1.contains("not registered"));
            }
            other => panic!("expected AllSourcesFailed, got {other:?}"),
        }
    }

}
