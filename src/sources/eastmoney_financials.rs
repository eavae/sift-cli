//! East Money (`eastmoney`) implementation of [`FinancialSource`].
//!
//! Covers three upstream API shapes behind a single source:
//!
//! - **A-share** (`Market::CnA`) — two-step PC_HSF10 endpoints
//!   ([`a_three`], [`indicator`]). Step 1 lists available report
//!   dates, step 2 pulls the wide-table rows. A `companyType` fallback
//!   chain (`4 → 3 → 2 → 1`) discovers which sector template the
//!   symbol uses; the result is cached per source instance.
//! - **HK** (`Market::Hk`) — datacenter `v1/get` long-table endpoints
//!   ([`hk_three`]). One summary call resolves `currency` /
//!   `period_type` per report date, then per-statement long-table
//!   calls join against that.
//! - **US** (`Market::Us`) — stubbed for this story; the public
//!   `supports` matrix admits US consolidated queries, but `fetch`
//!   currently returns an empty `Vec`. Story 05's smoke phase will
//!   decide whether to flesh out US live or keep it fixture-only.
//!
//! Field-name normalization runs through
//! [`crate::domain::items_dict::dict`] in every adapter — upstream
//! English columns (`TOTAL_OPERATE_INCOME`) and Chinese long-table
//! labels (`营业总收入`) both resolve to the same standardized item
//! name. Unknown columns pass through verbatim **and** get recorded
//! for the end-of-run hint via
//! [`crate::domain::items_dict::record_unmapped`].

pub mod a_three;
pub mod hk_three;
pub mod indicator;
pub mod translate;
pub mod us_three;

use std::collections::HashMap;
use std::sync::Mutex;

use crate::domain::market::Market;
use crate::domain::{FinancialRow, Query, Scope, Statement, Symbol};
use crate::error::SiftError;
use crate::http::HttpClient;
use crate::sources::financial_source::FinancialSource;

/// Production defaults — the real EM endpoints.
const DEFAULT_HSF10_BASE: &str =
    "https://emweb.securities.eastmoney.com/PC_HSF10/NewFinanceAnalysis";
const DEFAULT_DATACENTER_BASE: &str =
    "https://datacenter.eastmoney.com/securities/api/data/v1/get";

/// Base URLs for the two EM endpoint families. Tests inject mockito
/// URLs here; production reads optional env overrides via
/// [`EmUrls::from_env`] so Story 05's e2e tests can redirect without
/// touching the binary.
#[derive(Debug, Clone)]
pub struct EmUrls {
    /// A-share PC_HSF10 base (no trailing slash). Two paths get
    /// appended at request time: `/{slug}DateAjaxNew` and
    /// `/{slug}AjaxNew`.
    pub hsf10_base: String,
    /// HK / US datacenter `v1/get` endpoint. Query parameters are
    /// appended directly after `?`.
    pub datacenter_base: String,
}

impl EmUrls {
    pub fn from_env() -> Self {
        Self {
            hsf10_base: std::env::var("SIFT_EM_HSF10_BASE")
                .unwrap_or_else(|_| DEFAULT_HSF10_BASE.into()),
            datacenter_base: std::env::var("SIFT_EM_DATACENTER_BASE")
                .unwrap_or_else(|_| DEFAULT_DATACENTER_BASE.into()),
        }
    }
}

/// The single East Money source. Holds a per-instance `companyType`
/// cache so tests get a fresh state and parallel test runs do not
/// race on a global.
pub struct EastmoneyFinancialSource {
    urls: EmUrls,
    company_type_cache: Mutex<HashMap<Symbol, u8>>,
}

impl EastmoneyFinancialSource {
    pub fn new() -> Self {
        Self {
            urls: EmUrls::from_env(),
            company_type_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Construct with explicit URLs. Test-only seam pointing at
    /// mockito; production goes through [`Self::new`] which reads
    /// the env-resolved defaults. `#[cfg(test)]` keeps the symbol
    /// out of release binaries and out of dead-code lints.
    #[cfg(test)]
    pub fn with_urls(hsf10_base: impl Into<String>, datacenter_base: impl Into<String>) -> Self {
        Self {
            urls: EmUrls {
                hsf10_base: hsf10_base.into(),
                datacenter_base: datacenter_base.into(),
            },
            company_type_cache: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn urls(&self) -> &EmUrls {
        &self.urls
    }

    pub(crate) fn company_type_cache(&self) -> &Mutex<HashMap<Symbol, u8>> {
        &self.company_type_cache
    }
}

impl Default for EastmoneyFinancialSource {
    fn default() -> Self {
        Self::new()
    }
}

impl FinancialSource for EastmoneyFinancialSource {
    fn name(&self) -> &'static str {
        "eastmoney"
    }

    fn supports(&self, q: &Query) -> bool {
        match (q.symbol.market, q.scope) {
            (Market::CnA, _) => true,
            (Market::Hk | Market::Us, Scope::Consolidated) => true,
            (Market::Hk | Market::Us, Scope::Parent) => false,
        }
    }

    fn fetch(&self, q: &Query, http: &HttpClient) -> Result<Vec<FinancialRow>, SiftError> {
        match (q.symbol.market, q.statement) {
            (Market::CnA, Statement::Indicator) => indicator::fetch_a(self, q, http),
            (Market::CnA, _) => a_three::fetch(self, q, http),
            (Market::Hk, Statement::Indicator) => indicator::fetch_hk(self, q, http),
            (Market::Hk, _) => hk_three::fetch(self, q, http),
            (Market::Us, _) => us_three::fetch(self, q, http),
        }
    }

    /// A-share uses EM's income-statement date endpoint; HK uses the
    /// HKF10 cashflow summary endpoint (one HTTP call each, both reuse
    /// machinery from the corresponding `fetch` paths). US falls back
    /// to empty until `us_three::fetch` itself is unstubbed.
    fn list_periods(
        &self,
        symbol: &crate::domain::Symbol,
        http: &HttpClient,
    ) -> Result<Vec<crate::domain::Period>, SiftError> {
        match symbol.market {
            Market::CnA => a_three::list_periods_a(self, symbol, http),
            Market::Hk => hk_three::list_periods_hk(self, symbol, http),
            Market::Us => Ok(Vec::new()),
        }
    }
}

/// Factory returning a boxed source for the global registry. Story 05
/// calls this in `main` startup.
pub fn build() -> Box<dyn FinancialSource> {
    Box::new(EastmoneyFinancialSource::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Period;

    fn q(market: Market, statement: Statement, scope: Scope) -> Query {
        Query {
            symbol: Symbol {
                code: match market {
                    Market::CnA => "600519".into(),
                    Market::Hk => "00700".into(),
                    Market::Us => "AAPL".into(),
                },
                market,
                kind: crate::domain::market::InstrumentKind::Equity,
            },
            statement,
            periods: vec![Period::Annual(2024)],
            scope,
        }
    }

    #[test]
    fn supports_matrix() {
        let s = EastmoneyFinancialSource::new();

        // A-share: every statement / scope combination supported.
        assert!(s.supports(&q(Market::CnA, Statement::Income, Scope::Consolidated)));
        assert!(s.supports(&q(Market::CnA, Statement::Income, Scope::Parent)));
        assert!(s.supports(&q(Market::CnA, Statement::Balance, Scope::Parent)));
        assert!(s.supports(&q(Market::CnA, Statement::Indicator, Scope::Consolidated)));

        // HK: consolidated only.
        assert!(s.supports(&q(Market::Hk, Statement::Income, Scope::Consolidated)));
        assert!(!s.supports(&q(Market::Hk, Statement::Income, Scope::Parent)));
        assert!(!s.supports(&q(Market::Hk, Statement::Balance, Scope::Parent)));

        // US: consolidated only (even though `fetch` is stubbed for now).
        assert!(s.supports(&q(Market::Us, Statement::Income, Scope::Consolidated)));
        assert!(!s.supports(&q(Market::Us, Statement::Income, Scope::Parent)));
    }

    #[test]
    fn build_factory_returns_eastmoney_named_source() {
        let b = build();
        assert_eq!(b.name(), "eastmoney");
    }
}
