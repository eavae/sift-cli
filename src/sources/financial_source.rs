//! `FinancialSource` trait — the source contract only.
//!
//! Dispatch + per-source cache coordination lives in
//! [`crate::fetch::report`]; ambient state (HTTP client, caches,
//! source list) lives in [`crate::app::AppContext`] and is threaded
//! down by reference. The registered source list is now a plain
//! `Vec<Box<dyn FinancialSource>>` field on `AppContext`, populated
//! by `main.rs` — there is no process-global registry.
//!
//! `fetch` is the implementer's contract: each adapter owns its full
//! upstream pipeline — HTTP, JSON parsing, field translation, unit
//! conversion, and item-name normalization through
//! [`crate::domain::items_dict::dict`]. Implementers only see
//! [`AppContext`]; per-source cache prefilter / writeback is the
//! dispatcher's responsibility, not the source's.

use crate::app::AppContext;
use crate::domain::{FinancialRow, Period, Query, Symbol};
use crate::error::SiftError;

/// Compatibility alias used during the AppContext migration. Existing
/// source impls (`a_three`, `hk_three`, sina, …) keep using
/// `ctx: &Context`; new code should prefer `&AppContext` directly.
pub type Context = AppContext;

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
    fn fetch(&self, q: &Query, ctx: &Context) -> Result<Vec<FinancialRow>, SiftError>;

    /// List the report periods this source has available for `symbol`.
    /// Default implementation returns an empty `Vec` — sources that
    /// expose a dedicated "date list" endpoint (EM `lrbDateAjaxNew`,
    /// sina `type=0`) override this. Consumed by
    /// [`crate::fetch::report::list_periods_union`].
    fn list_periods(
        &self,
        _symbol: &Symbol,
        _ctx: &Context,
    ) -> Result<Vec<Period>, SiftError> {
        Ok(Vec::new())
    }
}
