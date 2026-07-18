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
/// HTTP transport, JSON / envelope parsing, and unit scaling. Item
/// labels are passed through **verbatim** — there is no name
/// dictionary, so each source emits whatever its upstream calls the
/// line item (EM: raw English column codes; HK / sina: native
/// Chinese). The dispatcher does **not** post-process the result —
/// what `fetch` returns is what the user sees, modulo the command
/// layer's sort and `--items` slice.
pub trait FinancialSource: Send + Sync {
    /// Short lower-case label woven into error messages and the
    /// `source` column (`"eastmoney"`, `"sina"`).
    fn name(&self) -> &'static str;

    /// `true` iff this source can answer `q`. Called once per
    /// dispatch in the hot path; must be cheap and side-effect-free.
    /// Returning `false` is how a source opts out of a request
    /// (e.g. sina returns `false` for `--scope parent`).
    fn supports(&self, q: &Query) -> bool;

    /// Whether this source joins the **automatic** first-success-wins
    /// race (the default, no `--source`). Sources that return `false`
    /// are reachable only when the user pins them with `--source
    /// <name>`. Default `true`.
    ///
    /// sina overrides this to `false`: it labels A-share items with
    /// Chinese names while EM uses raw English column codes, so racing
    /// both would make the field names depend on which source happened
    /// to answer first. Keeping sina opt-in gives the default path a
    /// single, stable label vocabulary (EM's).
    fn auto_dispatch(&self) -> bool {
        true
    }

    /// Fetch rows for `q` (item labels verbatim from upstream). See
    /// the trait docs for the implementer contract.
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
