//! Data-source layer. Each submodule encapsulates one upstream
//! provider; cache plumbing for these sources lives in
//! [`crate::cache`].
//!
//! - `cninfo` is the F1 search-listing source.
//! - `financial_source` defines the F2 trait every financial-data
//!   upstream implements, plus the first-success-wins dispatcher.
//!   Concrete adapters (eastmoney, sina) land in Stories 03 / 04.

pub mod cninfo;
pub mod eastmoney_financials;
pub mod financial_source;
pub mod sina_financials;
