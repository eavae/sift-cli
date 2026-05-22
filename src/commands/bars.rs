//! `sift bars <symbol>...` — historical daily K-lines (F5 story-02
//! single-symbol plus story-03 multi-symbol).
//!
//! The command does five things:
//! 1. Soft-reject `--format json` with a user-facing message.
//! 2. Fetch each symbol serially via
//!    `eastmoney::bars_daily_with_base`; collect failures into a
//!    Vec without writing stderr inside the loop.
//! 3. If every symbol fails return `SiftError::AllSourcesFailed`
//!    (exit code 3); stdout stays untouched.
//! 4. Otherwise dispatch on `fmt`:
//!    - `Table`: multi-symbol input renders grouped via
//!      `output::bars::render_grouped`; single-symbol input renders
//!      a plain long table to preserve story-02 behaviour exactly.
//!    - `Tsv`: always emits a flat long table (14 columns, symbols
//!      interleaved) through `output::render`.
//!    - `Json`: rejected at the entry — `unreachable!` here.
//! 5. After stdout is fully written, drain collected failures to
//!    stderr so warns never interleave with data.

use std::io::Write;

use clap::Args;
use time::format_description::well_known::Iso8601;
use time::{Date, Duration, OffsetDateTime};

use crate::app::AppContext;
use crate::domain::bars::Adjust;
use crate::domain::Symbol;
use crate::error::SiftError;
use crate::output::{self, Format};
use crate::sources::eastmoney::{self, BarsQuery, EmBarsUrls};

#[derive(Args, Debug)]
pub struct BarsArgs {
    /// One or more symbols (6 digits for CN A-share, 5 digits for
    /// HK; forms like `600519` or `00700.HK` are accepted).
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

    /// Take the most recent N trading days. Mutually exclusive with
    /// `--start` / `--end`.
    #[arg(long, conflicts_with_all = &["start", "end"])]
    pub limit: Option<usize>,

    /// Adjustment mode: `none` (no adjustment, default), `pre`
    /// (pre-adjusted), `post` (post-adjusted).
    #[arg(long, value_enum, default_value_t = AdjustArg::None)]
    pub adjust: AdjustArg,
}

/// `clap::ValueEnum` shadow of [`Adjust`] — the domain type already
/// carries a serde-driven NDJSON encoding, so attaching
/// `clap::ValueEnum` directly to it would conflict. The two enums
/// stay in sync through the `From` impl below.
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

pub fn run(args: BarsArgs, ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    if fmt == Format::Json {
        return Err(SiftError::Internal(
            "`--format json` is not supported by `sift bars`; \
             use `--format tsv` or omit `--format` for the default table"
                .into(),
        ));
    }

    let urls = EmBarsUrls::from_env();
    let (start, end) = resolve_date_range(&args)?;

    // Pass 1 — collect only; write nothing to stdout or stderr yet.
    let mut all_rows: Vec<crate::domain::bars::BarRow> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    let symbol_count = args.symbols.len();

    for raw in &args.symbols {
        let res = Symbol::parse(raw).and_then(|sym| {
            let q = BarsQuery {
                symbol: sym,
                start,
                end,
                limit: args.limit,
                adjust: args.adjust.into(),
            };
            eastmoney::bars_daily_with_base(&ctx.http, &q, &urls.bars_base)
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
                // Single-symbol: keep story-02 behaviour exactly —
                // plain long table, no `── … ───` group header.
                // Multi-symbol: group by symbol in input order, with
                // the group header carrying `symbol` / `adjust` /
                // `source` so the per-row columns stay tight.
                if symbol_count == 1 {
                    output::render(&mut handle, fmt, &all_rows)?;
                } else {
                    output::bars::render_grouped(&mut handle, &all_rows)?;
                }
            }
            Format::Tsv => output::render(&mut handle, fmt, &all_rows)?,
            Format::Json => unreachable!("rejected at run entry"),
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
/// today-365d … today.
fn resolve_date_range(args: &BarsArgs) -> Result<(Option<Date>, Option<Date>), SiftError> {
    if args.limit.is_some() {
        return Ok((None, None));
    }
    let today = OffsetDateTime::now_utc().date();
    let start = match &args.start {
        Some(s) => Some(parse_iso_date(s)?),
        None => Some(today.saturating_sub(Duration::days(365))),
    };
    let end = match &args.end {
        Some(s) => Some(parse_iso_date(s)?),
        None => Some(today),
    };
    Ok((start, end))
}

fn parse_iso_date(s: &str) -> Result<Date, SiftError> {
    Date::parse(s, &Iso8601::DATE).map_err(|e| {
        SiftError::Parse(format!("invalid date {s:?} (expected YYYY-MM-DD): {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpClient;

    fn ctx() -> AppContext {
        AppContext {
            http: HttpClient::new(),
            file_cache: None,
            records_cache: None,
        }
    }

    fn em_body(code: &str, lines: &[&str]) -> String {
        let arr = lines.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(",");
        format!(
            r#"{{"rc":0,"data":{{"code":"{code}","name":"X","klt":101,"klines":[{arr}]}}}}"#,
        )
    }

    fn args(symbols: Vec<&str>) -> BarsArgs {
        BarsArgs {
            symbols: symbols.into_iter().map(String::from).collect(),
            start: None,
            end: None,
            limit: Some(1),
            adjust: AdjustArg::None,
        }
    }

    #[test]
    fn json_format_is_soft_rejected_with_user_facing_message() {
        let err = run(args(vec!["600519"]), &ctx(), Format::Json).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, SiftError::Internal(_)));
        assert!(msg.contains("`sift bars`"), "msg: {msg}");
        assert!(!msg.contains("F5"), "msg should not leak codename: {msg}");
    }

    #[test]
    fn invalid_date_yields_parse_error() {
        let mut a = args(vec!["600519"]);
        a.limit = None;
        a.start = Some("2024-13-01".into());
        let err = run(a, &ctx(), Format::Tsv).unwrap_err();
        assert!(matches!(err, SiftError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn all_symbols_failing_yields_all_sources_failed() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/api/qt/stock/kline/get")
            .match_query(mockito::Matcher::Any)
            .with_status(404)
            .expect_at_least(2)
            .create();
        unsafe { std::env::set_var("SIFT_EM_BARS_BASE", server.url()); }

        let err = run(args(vec!["600519", "000001"]), &ctx(), Format::Table).unwrap_err();
        unsafe { std::env::remove_var("SIFT_EM_BARS_BASE"); }

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
        let mut server = mockito::Server::new();
        let _bad = server
            .mock("GET", "/api/qt/stock/kline/get")
            .match_query(mockito::Matcher::UrlEncoded("secid".into(), "0.999999".into()))
            .with_status(404)
            .create();
        let _ok = server
            .mock("GET", "/api/qt/stock/kline/get")
            .match_query(mockito::Matcher::UrlEncoded("secid".into(), "1.600519".into()))
            .with_status(200)
            .with_body(em_body("600519", &["2024-01-15,10,15,20,8,100,1000,1,2,3,0.1"]))
            .create();
        unsafe { std::env::set_var("SIFT_EM_BARS_BASE", server.url()); }

        let res = run(args(vec!["999999", "600519"]), &ctx(), Format::Tsv);
        unsafe { std::env::remove_var("SIFT_EM_BARS_BASE"); }
        assert!(res.is_ok(), "partial failure should still exit 0: {res:?}");
    }
}
