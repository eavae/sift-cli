//! Data-source layer. Each submodule encapsulates one upstream
//! provider; cache plumbing for these sources lives in
//! [`crate::cache`].
//!
//! - `cninfo` is the search-listing source.
//! - `financial_source` defines the trait every financial-data
//!   upstream implements; concrete adapters live in `eastmoney_financials`
//!   and `sina_financials`.

pub mod bars_source;
pub mod cninfo;
pub mod eastmoney;
pub mod eastmoney_financials;
pub mod financial_source;
pub mod paddleocr;
pub mod quote_source;
pub mod sina_financials;
pub mod tencent;
