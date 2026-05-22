//! Quote data-access coordinator.
//!
//! Owns the source selection layer for `sift quote`.
//! `commands/quote.rs` calls into the public entry point here
//! ([`dispatch_named`]); per-source URL building and parsing live
//! in `sources::eastmoney::quote` (the only registered source today
//! — Tencent / Sina additions land behind the same trait).
//!
//! ## Layering
//!
//! Same shape as [`crate::fetch::bars`]: a [`QuoteContext`] borrows
//! `&AppContext` and the registered source list; `dispatch_named`
//! pins to a single source by `name()`.

use crate::app::AppContext;
use crate::domain::quote::QuoteRow;
use crate::domain::Symbol;
use crate::error::SiftError;
use crate::sources::quote_source::QuoteSource;

pub struct QuoteContext<'a> {
    pub app: &'a AppContext,
    pub sources: &'a [Box<dyn QuoteSource>],
}

/// Pin the fetch to a single registered source by `name()`. With
/// only one source registered today (`eastmoney`), the typical
/// `name = "eastmoney"` path is the only one exercised, but the
/// dispatcher already supports the multi-source shape so adding a
/// Tencent / Sina implementer is a single-file change.
pub fn dispatch_named(
    symbol: &Symbol,
    ctx: &QuoteContext,
    name: &str,
) -> Result<QuoteRow, SiftError> {
    let Some(src) = ctx.sources.iter().find(|s| s.name() == name) else {
        let known: Vec<&str> = ctx.sources.iter().map(|s| s.name()).collect();
        return Err(SiftError::NoApplicableSource(format!(
            "--source {name} not registered; known sources: {}",
            known.join(", ")
        )));
    };
    src.quote(symbol, &ctx.app.http)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::Market;
    use crate::http::HttpClient;

    struct MockSource {
        name: &'static str,
    }
    impl QuoteSource for MockSource {
        fn name(&self) -> &'static str {
            self.name
        }
        fn quote(
            &self,
            symbol: &Symbol,
            _http: &HttpClient,
        ) -> Result<QuoteRow, SiftError> {
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
                source: self.name,
            })
        }
    }

    fn sym() -> Symbol {
        Symbol {
            code: "600519".into(),
            market: Market::CnA,
        }
    }

    #[test]
    fn dispatch_routes_to_named_source() {
        let app = AppContext::default();
        let sources: Vec<Box<dyn QuoteSource>> = vec![Box::new(MockSource { name: "eastmoney" })];
        let ctx = QuoteContext {
            app: &app,
            sources: &sources,
        };
        let row = dispatch_named(&sym(), &ctx, "eastmoney").unwrap();
        assert_eq!(row.source, "eastmoney");
    }

    #[test]
    fn unknown_source_yields_no_applicable_source() {
        let app = AppContext::default();
        let sources: Vec<Box<dyn QuoteSource>> = vec![Box::new(MockSource { name: "eastmoney" })];
        let ctx = QuoteContext {
            app: &app,
            sources: &sources,
        };
        let err = dispatch_named(&sym(), &ctx, "tencent").unwrap_err();
        assert!(matches!(err, SiftError::NoApplicableSource(_)));
    }
}
