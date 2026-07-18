//! Service layer — business logic between the view (`commands/`) and
//! storage (`store/`) / acquisition (`fetch/`, `sources/`).
//!
//! `service::facts` owns fact-store ingest logic; `service::metrics`
//! owns the controlled-vocabulary (`metric`) + mapping (`map`) logic;
//! `service::tsv` is the reusable `#`-header batch-parsing skeleton.
//! Commands call into here; they never reach `crate::store` directly.

pub mod facts;
pub mod metrics;
pub mod tsv;

use crate::app::AppContext;
use crate::error::SiftError;
use crate::store::FactStore;

/// Shared accessor: the fact-store handle, or a clear "unavailable"
/// error when `$HOME` / the DuckDB open failed at startup.
pub(crate) fn store(app: &AppContext) -> Result<&FactStore, SiftError> {
    app.facts.as_ref().ok_or_else(|| {
        SiftError::Io("fact store unavailable (could not resolve ~/.sift/facts.duckdb)".into())
    })
}
