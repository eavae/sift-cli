//! `sift quote <symbol>...` — current-price snapshot.
//!
//! The command does four things:
//! 1. Serially call [`crate::fetch::quote::dispatch_named`] for
//!    each symbol — the command does **not** import `sources::*`
//!    directly; per-source URL building and parsing live behind
//!    the [`crate::sources::quote_source::QuoteSource`] trait.
//! 2. Render successful rows to stdout in one shot (table / TSV /
//!    NDJSON all via [`output::render`]) — the per-symbol
//!    loop deliberately writes **nothing** to stderr so data and
//!    diagnostic output never interleave.
//! 3. After stdout is fully written, emit `[warn] quote <symbol>:
//!    <cause>` lines for any per-symbol failure to stderr.
//!
//! If every symbol fails we do not write any stdout at all and
//! instead return `AllSourcesFailed` — the error object itself is
//! the report, and `main.rs` formats it onto stderr while exiting
//! with code 3 (per the `error::exit_code` table).

use std::io::Write;

use clap::Args;

use crate::domain::Symbol;
use crate::error::SiftError;
use crate::fetch::quote::{dispatch_named, QuoteContext};
use crate::output::{self, Format};

#[derive(Args, Debug)]
pub struct QuoteArgs {
    /// One or more symbols (6 digits for CN A-share, 5 digits for
    /// HK; forms like `600519`, `sh600519`, `600519.SH`,
    /// `00700.HK` are all accepted; indices use an explicit
    /// exchange prefix — `sh000001` = 上证指数, `sz399001` =
    /// 深证成指). Multiple symbols are fetched serially; per-symbol
    /// failure surfaces as a `[warn]` line on stderr without
    /// aborting the run.
    #[arg(required = true)]
    pub symbols: Vec<String>,
}

/// Currently only one quote source is registered (EM). We still
/// pin by name so adding Tencent / Sina later is a single-file
/// change in `main::run_quote` plus the source impl — the command
/// layer does not need to grow.
const DEFAULT_QUOTE_SOURCE: &str = "eastmoney";

pub fn run(args: QuoteArgs, ctx: &QuoteContext, fmt: Format) -> Result<(), SiftError> {
    let mut rows = Vec::with_capacity(args.symbols.len());
    let mut failures: Vec<(String, String)> = Vec::new();

    // Pass 1 — collect only; write nothing to stderr or stdout yet.
    for raw in &args.symbols {
        let res = Symbol::parse(raw).and_then(|sym| dispatch_named(&sym, ctx, DEFAULT_QUOTE_SOURCE));
        match res {
            Ok(row) => rows.push(row),
            Err(e) => failures.push((raw.clone(), e.to_string())),
        }
    }

    // All failed: skip stdout entirely; the error is the report.
    if rows.is_empty() {
        return Err(SiftError::AllSourcesFailed(
            failures
                .into_iter()
                .map(|(sym, cause)| (format!("quote {sym}"), cause))
                .collect(),
        ));
    }

    // Pass 2 — write all of stdout in one go. The lock is released
    // when this scope ends so the trailing stderr warns can never
    // interleave with stdout bytes.
    {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        output::render(&mut handle, fmt, &rows)?;
    }

    // Pass 3 — partial failure: drain the failure list to stderr
    // only after stdout is fully flushed.
    if !failures.is_empty() {
        let stderr = std::io::stderr();
        let mut err = stderr.lock();
        for (sym, cause) in &failures {
            let _ = writeln!(err, "[warn] quote {sym}: {cause}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppContext;
    use crate::domain::quote::QuoteRow;
    use crate::http::HttpClient;
    use crate::sources::quote_source::QuoteSource;

    /// Test seam matching the EM source's `name()` so `dispatch_named`
    /// finds it under the default source key.
    struct MockSource;
    impl QuoteSource for MockSource {
        fn name(&self) -> &'static str {
            DEFAULT_QUOTE_SOURCE
        }
        fn quote(
            &self,
            symbol: &Symbol,
            _http: &HttpClient,
        ) -> Result<QuoteRow, SiftError> {
            if symbol.code.starts_with("999") {
                return Err(SiftError::NotFound(symbol.code.clone()));
            }
            Ok(QuoteRow {
                symbol: format!("{}.{}", symbol.code, symbol.market.as_upper()),
                name: "MOCK".into(),
                price: 1.0,
                change: 0.0,
                pct_change: 0.0,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                prev_close: 1.0,
                volume: 0,
                amount: 0.0,
                time: "1970-01-01 00:00:00".into(),
                source: "eastmoney",
            })
        }
    }

    #[test]
    fn json_format_is_accepted_and_renders_ndjson() {
        // `--format json` goes through the generic RenderRow → NDJSON
        // pipeline like every other command; run() must not reject it.
        let app = AppContext::default();
        let sources: Vec<Box<dyn QuoteSource>> = vec![Box::new(MockSource)];
        let ctx = QuoteContext {
            app: &app,
            sources: &sources,
        };
        let args = QuoteArgs {
            symbols: vec!["600519".into()],
        };
        let res = run(args, &ctx, Format::Json);
        assert!(res.is_ok(), "json format should be accepted: {res:?}");
    }

    #[test]
    fn all_symbols_failing_yields_all_sources_failed() {
        let app = AppContext::default();
        let sources: Vec<Box<dyn QuoteSource>> = vec![Box::new(MockSource)];
        let ctx = QuoteContext {
            app: &app,
            sources: &sources,
        };
        let args = QuoteArgs {
            symbols: vec!["999999".into(), "999998".into()],
        };
        let err = run(args, &ctx, Format::Table).unwrap_err();
        match err {
            SiftError::AllSourcesFailed(v) => {
                assert_eq!(v.len(), 2);
                assert!(v[0].0.starts_with("quote "));
            }
            other => panic!("expected AllSourcesFailed, got {other:?}"),
        }
    }

    #[test]
    fn partial_failure_returns_ok() {
        let app = AppContext::default();
        let sources: Vec<Box<dyn QuoteSource>> = vec![Box::new(MockSource)];
        let ctx = QuoteContext {
            app: &app,
            sources: &sources,
        };
        let args = QuoteArgs {
            symbols: vec!["999999".into(), "600519".into()],
        };
        let res = run(args, &ctx, Format::Tsv);
        assert!(res.is_ok(), "partial failure should exit 0: {res:?}");
    }
}
