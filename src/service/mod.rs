//! Service layer — business logic between the view (`commands/`) and
//! storage (`store/`) / acquisition (`fetch/`, `sources/`).
//!
//! `service::facts` owns the fact-store logic (TSV parse/validate,
//! row mapping, best-effort ingest, SQL forwarding). `service::tsv`
//! is the reusable `#`-header batch-parsing skeleton. Commands call
//! into here; they never reach `crate::store` directly.

pub mod facts;
pub mod tsv;
