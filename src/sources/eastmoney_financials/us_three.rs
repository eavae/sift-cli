//! US three statements via the EM datacenter.
//!
//! Stubbed for this story: the public `supports` matrix admits
//! `(Market::Us, _, Scope::Consolidated)`, but `fetch` here returns an
//! empty `Vec`. The story explicitly allows this — Story 05's smoke
//! phase will decide whether to flesh out US live or keep it
//! fixture-only.
//!
//! Concretely: the production EM datacenter URL family for US is the
//! same `v1/get` shape as HK, with `reportName = RPT_F10_*` instead
//! of `RPT_HKF10_*`. Wiring that here is straightforward once we have
//! confirmed `reportName` values from real responses; until then,
//! returning empty rows lets `dispatch` propagate "no data" to the
//! command layer without injecting plausibly-wrong values.

use crate::domain::{FinancialRow, Query};
use crate::error::SiftError;
use crate::sources::financial_source::Context;

use super::EastmoneyFinancialSource;

pub(crate) fn fetch(
    _src: &EastmoneyFinancialSource,
    _q: &Query,
    _ctx: &Context,
) -> Result<Vec<FinancialRow>, SiftError> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::Market;
    use crate::domain::{Period, Scope, Statement, Symbol};

    #[test]
    fn us_fetch_returns_empty_for_now() {
        let src = EastmoneyFinancialSource::with_urls("http://unused", "http://unused");
        let q = Query {
            symbol: Symbol {
                code: "AAPL".into(),
                market: Market::Us,
            },
            statement: Statement::Income,
            periods: vec![Period::Annual(2024)],
            scope: Scope::Consolidated,
        };
        let rows = fetch(&src, &q, &Context::default()).unwrap();
        assert!(rows.is_empty());
    }
}
