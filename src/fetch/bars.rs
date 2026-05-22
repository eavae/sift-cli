//! Bars data-access coordinator.
//!
//! Owns the source selection layer for `sift bars`.
//! `commands/bars.rs` calls into the public entry point here
//! ([`dispatch_named`]); per-source URL building and parsing live
//! in `sources::{tencent,eastmoney}::bars`.
//!
//! ## Layering
//!
//! `fetch::bars` reads the source list from [`BarsContext::sources`];
//! `main.rs` builds the list and threads `&BarsContext` down. There
//! is no global registry — tests inject a source list by
//! constructing their own [`BarsContext`].
//!
//! ## Why not first-success-wins
//!
//! `fetch::report` races every applicable source and takes the first
//! `Ok`. Bars does not: the unified [`BarRow`] schema permits either
//! source-native fields (EM) or client-computed approximations
//! (Tencent), and silently mixing them across rows would produce
//! `amount` columns that drift between exact and approximate
//! without the user knowing. So `dispatch_named` always pins to a
//! single source (selected via `--source <name>`, default
//! `tencent`); callers that want EM's fields specifically must
//! opt in explicitly.

use crate::app::AppContext;
use crate::domain::bars::{BarRow, BarsQuery};
use crate::error::SiftError;
use crate::sources::bars_source::BarsSource;

/// Bars ambient state: the cross-cutting [`AppContext`] plus the
/// registered bars source list. Constructed by `main::run_bars` and
/// passed through `commands::bars::run` down into the dispatch
/// function. Borrowed everywhere — `app` and `sources` each live in
/// their own slot on `main`'s stack frame.
pub struct BarsContext<'a> {
    pub app: &'a AppContext,
    pub sources: &'a [Box<dyn BarsSource>],
}

/// Pin the fetch to a single registered source by `name()`. An
/// unknown name surfaces as [`SiftError::NoApplicableSource`]
/// listing the registered sources, so the user always sees what
/// is available.
pub fn dispatch_named(
    query: &BarsQuery,
    ctx: &BarsContext,
    name: &str,
) -> Result<Vec<BarRow>, SiftError> {
    let Some(src) = ctx.sources.iter().find(|s| s.name() == name) else {
        let known: Vec<&str> = ctx.sources.iter().map(|s| s.name()).collect();
        return Err(SiftError::NoApplicableSource(format!(
            "--source {name} not registered; known sources: {}",
            known.join(", ")
        )));
    };
    src.fetch(query, &ctx.app.http)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::bars::{Adjust, BarRow, Period};
    use crate::domain::market::Market;
    use crate::domain::Symbol;
    use crate::http::HttpClient;

    /// Minimal mock source returning a fixed row.
    struct MockSource {
        name: &'static str,
        row: BarRow,
    }
    impl BarsSource for MockSource {
        fn name(&self) -> &'static str {
            self.name
        }
        fn fetch(
            &self,
            _q: &BarsQuery,
            _http: &HttpClient,
        ) -> Result<Vec<BarRow>, SiftError> {
            Ok(vec![self.row.clone()])
        }
    }

    fn fixture_row(source: &'static str) -> BarRow {
        BarRow {
            symbol: "600519.CN-A".into(),
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
            adjust: Adjust::Pre,
            period: Period::Daily,
            source,
        }
    }

    fn query() -> BarsQuery {
        BarsQuery {
            symbol: Symbol {
                code: "600519".into(),
                market: Market::CnA,
            },
            start: None,
            end: None,
            limit: None,
            adjust: Adjust::Pre,
            period: Period::Daily,
        }
    }

    #[test]
    fn dispatch_routes_to_matching_source() {
        let app = AppContext::default();
        let sources: Vec<Box<dyn BarsSource>> = vec![
            Box::new(MockSource {
                name: "tencent",
                row: fixture_row("tencent"),
            }),
            Box::new(MockSource {
                name: "eastmoney",
                row: fixture_row("eastmoney"),
            }),
        ];
        let ctx = BarsContext {
            app: &app,
            sources: &sources,
        };

        let rows = dispatch_named(&query(), &ctx, "tencent").unwrap();
        assert_eq!(rows[0].source, "tencent");

        let rows = dispatch_named(&query(), &ctx, "eastmoney").unwrap();
        assert_eq!(rows[0].source, "eastmoney");
    }

    #[test]
    fn unknown_source_yields_no_applicable_source() {
        let app = AppContext::default();
        let sources: Vec<Box<dyn BarsSource>> = vec![Box::new(MockSource {
            name: "tencent",
            row: fixture_row("tencent"),
        })];
        let ctx = BarsContext {
            app: &app,
            sources: &sources,
        };
        let err = dispatch_named(&query(), &ctx, "sina").unwrap_err();
        match err {
            SiftError::NoApplicableSource(msg) => {
                assert!(msg.contains("sina"));
                assert!(msg.contains("tencent"), "msg should list known: {msg}");
            }
            other => panic!("expected NoApplicableSource, got {other:?}"),
        }
    }
}
