//! `FinancialSource` trait — the source contract only.
//!
//! Dispatch + per-source cache coordination lives in
//! [`crate::fetch::report`]. The trait deliberately takes only an
//! [`HttpClient`] — sources are pure HTTP-bound implementations,
//! they do not see [`AppContext`] / caches / the source list. Any
//! cache prefilter or writeback is the dispatcher's job; expanding
//! the trait surface (to e.g. let a source maintain its own cache)
//! is a future-broadening decision that should be made deliberately,
//! not by leaking ambient state through the function signature.

use crate::domain::{FinancialRow, Period, Query, Symbol};
use crate::error::SiftError;
use crate::http::HttpClient;

/// One financial-data upstream.
///
/// Implementers own the full upstream → [`FinancialRow`] pipeline:
/// HTTP transport, JSON / envelope parsing, field translation, unit
/// scaling, and item-name normalization via
/// [`crate::domain::items_dict::dict`]. The dispatcher does **not**
/// post-process the result — what `fetch` returns is what the user
/// sees, modulo the command layer's sort and `--items` slice.
pub trait FinancialSource: Send + Sync {
    /// Short lower-case label woven into error messages and the
    /// `source` column (`"eastmoney"`, `"sina"`).
    fn name(&self) -> &'static str;

    /// `true` iff this source can answer `q`. Called once per
    /// dispatch in the hot path; must be cheap and side-effect-free.
    /// Returning `false` is how a source opts out of a request
    /// (e.g. sina returns `false` for `--scope parent`).
    fn supports(&self, q: &Query) -> bool;

    /// Fetch normalized rows for `q`. See the trait docs for the
    /// implementer contract.
    fn fetch(&self, q: &Query, http: &HttpClient) -> Result<Vec<FinancialRow>, SiftError>;

    /// List the report periods this source has available for `symbol`.
    /// Default implementation returns an empty `Vec` — sources that
    /// expose a dedicated "date list" endpoint (EM `lrbDateAjaxNew`,
    /// sina `type=0`) override this. Consumed by
    /// [`crate::fetch::report::list_periods_union`].
    fn list_periods(
        &self,
        _symbol: &Symbol,
        _http: &HttpClient,
    ) -> Result<Vec<Period>, SiftError> {
        Ok(Vec::new())
    }
}
