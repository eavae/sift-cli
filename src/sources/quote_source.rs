//! `QuoteSource` trait — single-symbol current-price snapshot.
//!
//! Symmetric to [`crate::sources::bars_source::BarsSource`]: pure
//! HTTP-bound implementations, no caches, no [`AppContext`] leak,
//! one method (`quote`). Dispatch + source selection lives in
//! [`crate::fetch::quote`].

use crate::domain::quote::QuoteRow;
use crate::domain::Symbol;
use crate::error::SiftError;
use crate::http::HttpClient;

/// One snapshot-quote upstream. Today there is a single registered
/// implementer (EM `push2delay`); the trait is in place so a future
/// Tencent / sina addition can drop in without changing command-
/// layer code.
pub trait QuoteSource: Send + Sync {
    /// Short lowercase label, returned in the `source` column of
    /// emitted [`QuoteRow`] values and accepted by `--source`.
    fn name(&self) -> &'static str;

    /// Fetch the current-price snapshot for `symbol`.
    fn quote(&self, symbol: &Symbol, http: &HttpClient) -> Result<QuoteRow, SiftError>;
}
