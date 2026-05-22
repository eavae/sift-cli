//! `sift quote <symbol>...` — current-price snapshot (F5 story-01).
//!
//! The command does four things:
//! 1. Soft-reject `--format json` with a user-facing message that
//!    names the command (no internal codenames leak through).
//! 2. Serially call `sources::eastmoney::quote_with_base` for each
//!    symbol.
//! 3. Render successful rows to stdout in one shot — the per-symbol
//!    loop deliberately writes **nothing** to stderr so data and
//!    diagnostic output never interleave.
//! 4. After stdout is fully written, emit `[warn] quote <symbol>:
//!    <cause>` lines for any per-symbol failure to stderr.
//!
//! If every symbol fails we do not write any stdout at all and
//! instead return `AllSourcesFailed` — the error object itself is
//! the report, and `main.rs` formats it onto stderr while exiting
//! with code 3 (per the `error::exit_code` table).

use std::io::Write;

use clap::Args;

use crate::app::AppContext;
use crate::domain::Symbol;
use crate::error::SiftError;
use crate::output::{self, Format};
use crate::sources::eastmoney;

#[derive(Args, Debug)]
pub struct QuoteArgs {
    /// One or more symbols (6 digits for CN A-share, 5 digits for
    /// HK; forms like `600519`, `sh600519`, `600519.SH`,
    /// `00700.HK` are all accepted). Multiple symbols are fetched
    /// serially; per-symbol failure surfaces as a `[warn]` line on
    /// stderr without aborting the run.
    #[arg(required = true)]
    pub symbols: Vec<String>,
}

pub fn run(args: QuoteArgs, ctx: &AppContext, fmt: Format) -> Result<(), SiftError> {
    if fmt == Format::Json {
        return Err(SiftError::Internal(
            "`--format json` is not supported by `sift quote`; \
             use `--format tsv` or omit `--format` for the default table"
                .into(),
        ));
    }

    let urls = eastmoney::EmQuoteUrls::from_env();
    let mut rows = Vec::with_capacity(args.symbols.len());
    let mut failures: Vec<(String, String)> = Vec::new();

    // Pass 1 — collect only; write nothing to stderr or stdout yet.
    for raw in &args.symbols {
        let res = Symbol::parse(raw)
            .and_then(|sym| eastmoney::quote_with_base(&ctx.http, &sym, &urls.quote_base));
        match res {
            Ok(row) => rows.push(row),
            Err(e) => failures.push((raw.clone(), e.to_string())),
        }
    }

    // All failed: skip stdout entirely; the error is the report.
    // `main.rs` exits with code 3 (per `error::exit_code`).
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
    use crate::http::HttpClient;

    fn ctx() -> AppContext {
        AppContext {
            http: HttpClient::new(),
            file_cache: None,
            records_cache: None,
        }
    }

    fn em_body(name: &str, code: &str) -> String {
        format!(
            r#"{{"rc":0,"data":{{
                "f43":132359,"f44":133299,"f45":132000,"f46":132100,
                "f47":12674,"f48":1680717318.0,
                "f57":"{code}","f58":"{name}","f60":133069,
                "f86":1747724400,"f169":-710,"f170":-53
            }}}}"#,
        )
    }

    /// `--format json` soft-reject: returns `Internal`, the message
    /// names the command, no internal codename leaks out. Clap will
    /// not reject `Format::Json` itself because `--format` is a
    /// global value parser that accepts `json`; the per-subcommand
    /// rejection has to be a runtime check.
    #[test]
    fn json_format_is_soft_rejected_with_user_facing_message() {
        let args = QuoteArgs {
            symbols: vec!["600519".into()],
        };
        let err = run(args, &ctx(), Format::Json).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, SiftError::Internal(_)));
        assert!(
            msg.contains("`sift quote`"),
            "msg must name the user-facing command: {msg}"
        );
        assert!(!msg.contains("F5"), "msg must not leak internal codename: {msg}");
        assert!(msg.contains("tsv"), "msg should mention the supported format: {msg}");
    }

    /// All symbols fail: `run` returns `AllSourcesFailed` (mapped to
    /// exit code 3 by `error::exit_code`). We do not assert on
    /// stdout here directly — `run` returning `Err` already means
    /// `main.rs` will not have flushed any stdout — but the
    /// contract is enforced end-to-end by the assert_cmd test in
    /// `tests/quote_e2e.rs`.
    #[test]
    fn all_symbols_failing_yields_all_sources_failed() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/api/qt/stock/get")
            .match_query(mockito::Matcher::Any)
            .with_status(404)
            .expect_at_least(2)
            .create();

        // SAFETY: process-global env mutation. `cargo test` runs
        // tests inside one module on a single thread, and other
        // modules (sina / eastmoney_financials) use disjoint env
        // keys, so we cannot race with them either (see CLAUDE.md
        // "Test-only URL injection").
        unsafe { std::env::set_var("SIFT_EM_QUOTE_BASE", server.url()); }

        let args = QuoteArgs {
            symbols: vec!["600519".into(), "000001".into()],
        };
        let err = run(args, &ctx(), Format::Table).unwrap_err();
        match err {
            SiftError::AllSourcesFailed(v) => {
                assert_eq!(v.len(), 2, "both failures collected");
                assert!(v[0].0.starts_with("quote "));
            }
            other => panic!("expected AllSourcesFailed, got {other:?}"),
        }

        unsafe { std::env::remove_var("SIFT_EM_QUOTE_BASE"); }
    }

    /// Partial failure: first symbol 200, second symbol 404 → `run`
    /// returns Ok (stdout carries the successful row). We cannot
    /// capture stdout / stderr text from this direct call — the
    /// stdout-clean / stderr-trails-stdout invariants are covered
    /// by the assert_cmd e2e tests. Here we only assert that the
    /// happy path does not short-circuit into `AllSourcesFailed`.
    #[test]
    fn partial_failure_returns_ok_and_does_not_short_circuit() {
        let mut server = mockito::Server::new();
        let _bad = server
            .mock("GET", "/api/qt/stock/get")
            .match_query(mockito::Matcher::UrlEncoded("secid".into(), "0.999999".into()))
            .with_status(404)
            .expect_at_least(1)
            .create();
        let _ok = server
            .mock("GET", "/api/qt/stock/get")
            .match_query(mockito::Matcher::UrlEncoded("secid".into(), "1.600519".into()))
            .with_status(200)
            .with_body(em_body("贵州茅台", "600519"))
            .expect_at_least(1)
            .create();

        unsafe { std::env::set_var("SIFT_EM_QUOTE_BASE", server.url()); }
        let args = QuoteArgs {
            symbols: vec!["999999".into(), "600519".into()],
        };
        let res = run(args, &ctx(), Format::Tsv);
        unsafe { std::env::remove_var("SIFT_EM_QUOTE_BASE"); }

        assert!(res.is_ok(), "partial failure should still exit 0: {res:?}");
    }
}
