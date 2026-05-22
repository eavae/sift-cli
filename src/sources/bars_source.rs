//! `BarsSource` trait — the source contract only.
//!
//! Dispatch + source selection lives in
//! [`crate::fetch::bars`]. The trait takes only an [`HttpClient`]
//! and a [`BarsQuery`] — sources are pure HTTP-bound implementations
//! and never see [`crate::app::AppContext`] / caches / the source
//! list. Mirrors the [`crate::sources::financial_source::FinancialSource`]
//! pattern so the two layers stay structurally aligned.

use crate::domain::bars::{BarRow, BarsQuery};
use crate::error::SiftError;
use crate::http::HttpClient;

/// One historical-K-line upstream.
///
/// Implementers own the full upstream → [`BarRow`] pipeline: URL
/// construction, HTTP transport, JSON parsing, field reordering
/// (OCHL → OHLC), unit scaling (hands → shares), and computing the
/// derived fields (`amount` / `pct_change` / `change` /
/// `amplitude_pct`) when the upstream does not return them natively.
/// What `fetch::bars` returns is what the user sees, modulo the
/// command layer's grouping and `--limit` slice.
pub trait BarsSource: Send + Sync {
    /// Short lowercase label used by `--source <name>` and stored in
    /// the `source` column of each emitted row (`"tencent"`,
    /// `"eastmoney"`).
    fn name(&self) -> &'static str;

    /// Fetch normalized bars for `q`. Errors bubble through to the
    /// command layer's three-pass failure aggregation; sources do
    /// not retry beyond what [`HttpClient`] already does for 5xx.
    fn fetch(&self, q: &BarsQuery, http: &HttpClient) -> Result<Vec<BarRow>, SiftError>;
}
