//! `FinancialSource` trait + first-success-wins dispatcher.
//!
//! Every upstream (`EastMoney`, `Sina`, ...) implements
//! [`FinancialSource`] and is registered once at process startup via
//! [`init`]. [`dispatch`] picks every source whose `supports(query)`
//! returns true, runs them in parallel with `std::thread::scope`, and
//! returns the **first** `Ok` it receives. If every applicable source
//! returns `Err` (or panics), the result is
//! [`SiftError::AllSourcesFailed`] carrying one `(name, message)`
//! pair per source.
//!
//! `fetch` is the implementer's contract: each adapter owns its full
//! upstream pipeline — HTTP, JSON parsing, field translation, unit
//! conversion, and item-name normalization through
//! [`crate::domain::items_dict::dict`]. The dispatcher consumes only
//! normalized [`FinancialRow`] values.
//!
//! # Example
//!
//! ```ignore
//! use sift::sources::financial_source::{dispatch, init, Context};
//! init(vec![sources::eastmoney::build(), sources::sina::build()]);
//! let rows = dispatch(&query, &Context::default())?;
//! ```

// Story 02 ships the trait + dispatcher as the seam every subsequent
// F2 story (03 EM source, 04 sina source, 05 commands) plugs into; the
// public surface here is unused inside the binary until Story 05 wires
// it through `commands::financials`. Suppress dead-code warnings until
// then so `cargo build` stays clean.
#![allow(dead_code)]

use std::sync::{mpsc, Arc, OnceLock};

use crate::cache::financials::FinancialCache;
use crate::domain::{FinancialRow, Period, Query, Symbol};
use crate::domain::SourceTag;
use crate::error::SiftError;
use crate::http::HttpClient;

/// Shared state every source receives on every dispatch. Holds the
/// HTTP client and an optional financial-row cache (Story 05).
/// `cache = None` means "no caching" — the unit tests that exercise
/// dispatch without a tempdir-backed DuckDB use this path.
///
/// `Default` is derived: `HttpClient::default` calls `new`, and
/// `Option::default` is `None` — together that gives the no-cache
/// in-memory context the tests want.
#[derive(Default)]
pub struct Context {
    pub http: HttpClient,
    pub cache: Option<Arc<FinancialCache>>,
}

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
    /// sina `type=0`) override this. Called by the `sift financials
    /// periods` subcommand.
    fn list_periods(
        &self,
        _symbol: &Symbol,
        _ctx: &Context,
    ) -> Result<Vec<Period>, SiftError> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

static REGISTRY: OnceLock<Vec<Box<dyn FinancialSource>>> = OnceLock::new();

/// Install the process-global source registry. Must be called exactly
/// once early in `main`; a second call panics.
pub fn init(sources: Vec<Box<dyn FinancialSource>>) {
    REGISTRY
        .set(sources)
        .map_err(|_| ())
        .expect("financial_source::init called twice");
}

/// Borrow the registered sources. Panics if [`init`] was never called.
pub fn registry() -> &'static [Box<dyn FinancialSource>] {
    REGISTRY
        .get()
        .expect("financial_source::init not called yet")
        .as_slice()
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Production entry. Runs every applicable registered source in
/// parallel and returns the first `Ok`. See the module-level docs for
/// failure semantics.
pub fn dispatch(query: &Query, ctx: &Context) -> Result<Vec<FinancialRow>, SiftError> {
    let sources: Vec<&dyn FinancialSource> =
        registry().iter().map(|b| b.as_ref()).collect();
    dispatch_against(query, ctx, &sources)
}

/// Lower-level entry: dispatch against an explicit slice of sources.
/// Used both by [`dispatch`] (passing the global registry) and by
/// tests (passing mocks without touching the `OnceLock`).
pub fn dispatch_against(
    query: &Query,
    ctx: &Context,
    sources: &[&dyn FinancialSource],
) -> Result<Vec<FinancialRow>, SiftError> {
    let applicable: Vec<&dyn FinancialSource> = sources
        .iter()
        .copied()
        .filter(|s| s.supports(query))
        .collect();

    if applicable.is_empty() {
        return Err(SiftError::NoApplicableSource(format!(
            "symbol={} statement={} scope={}",
            query.symbol.code,
            query.statement.as_str(),
            query.scope.as_str(),
        )));
    }

    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel::<(String, Result<Vec<FinancialRow>, SiftError>)>();

        for &src in &applicable {
            let tx = tx.clone();
            let name = src.name();
            scope.spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    src.fetch(query, ctx)
                }))
                .unwrap_or_else(|payload| {
                    Err(SiftError::Network(format!(
                        "{name} panicked: {}",
                        panic_message(payload)
                    )))
                });
                let _ = tx.send((name.to_string(), result));
            });
        }
        // Drop the original tx so `rx.recv()` returns Err once every
        // worker has dropped its clone, signalling the channel is
        // fully drained.
        drop(tx);

        let mut failures: Vec<(String, String)> = Vec::with_capacity(applicable.len());
        while let Ok((name, result)) = rx.recv() {
            match result {
                Ok(rows) => return Ok(rows),
                Err(e) => failures.push((name, e.to_string())),
            }
        }
        Err(SiftError::AllSourcesFailed(failures))
    })
}

// ---------------------------------------------------------------------------
// Cache-aware dispatch (Story 05)
// ---------------------------------------------------------------------------

/// `dispatch` + per-source cache. Each applicable source first
/// consults `ctx.cache` for every period in `query.periods`; cached
/// periods are served from disk and only the misses go to HTTP. On
/// success the missing periods are upserted back into the cache.
///
/// Returns the first source that completes successfully — same
/// first-success-wins semantics as [`dispatch`]; the cache is just a
/// pre-filter that lets a fully-cached source return ~instantly.
pub fn dispatch_with_cache(query: &Query, ctx: &Context) -> Result<Vec<FinancialRow>, SiftError> {
    let sources: Vec<&dyn FinancialSource> =
        registry().iter().map(|b| b.as_ref()).collect();
    dispatch_with_cache_against(query, ctx, &sources)
}

/// Test seam mirroring [`dispatch_against`]: same as
/// [`dispatch_with_cache`] but takes an explicit source slice instead
/// of the global registry.
pub fn dispatch_with_cache_against(
    query: &Query,
    ctx: &Context,
    sources: &[&dyn FinancialSource],
) -> Result<Vec<FinancialRow>, SiftError> {
    let applicable: Vec<&dyn FinancialSource> = sources
        .iter()
        .copied()
        .filter(|s| s.supports(query))
        .collect();

    if applicable.is_empty() {
        return Err(SiftError::NoApplicableSource(format!(
            "symbol={} statement={} scope={}",
            query.symbol.code,
            query.statement.as_str(),
            query.scope.as_str(),
        )));
    }

    let today = today_utc();

    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel::<(String, Result<Vec<FinancialRow>, SiftError>)>();

        for &src in &applicable {
            let tx = tx.clone();
            let name = src.name();
            scope.spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    fetch_with_per_source_cache(src, query, ctx, today)
                }))
                .unwrap_or_else(|payload| {
                    Err(SiftError::Network(format!(
                        "{name} panicked: {}",
                        panic_message(payload)
                    )))
                });
                let _ = tx.send((name.to_string(), result));
            });
        }
        drop(tx);

        let mut failures: Vec<(String, String)> = Vec::with_capacity(applicable.len());
        while let Ok((name, result)) = rx.recv() {
            match result {
                Ok(rows) => return Ok(rows),
                Err(e) => failures.push((name, e.to_string())),
            }
        }
        Err(SiftError::AllSourcesFailed(failures))
    })
}

/// Like [`dispatch_with_cache`] but restricted to a single named
/// source. `name = None` falls back to the standard
/// first-success-wins behavior across every applicable source —
/// useful so callers can collapse `--source auto / eastmoney / sina`
/// into one call site.
///
/// An unknown name surfaces as `NoApplicableSource` listing the
/// registered sources; this guards against a binary built without the
/// source the caller asked for.
pub fn dispatch_with_cache_named(
    query: &Query,
    ctx: &Context,
    name: Option<&str>,
) -> Result<Vec<FinancialRow>, SiftError> {
    let Some(want) = name else {
        return dispatch_with_cache(query, ctx);
    };
    let reg = registry();
    let Some(src) = reg.iter().find(|s| s.name() == want) else {
        let known: Vec<&str> = reg.iter().map(|s| s.name()).collect();
        return Err(SiftError::NoApplicableSource(format!(
            "--source {want} not registered; known sources: {}",
            known.join(", ")
        )));
    };
    dispatch_with_cache_against(query, ctx, &[src.as_ref()])
}

/// One source's cache-aware fetch: hit cache for each period, fetch
/// only the misses, write fresh results back, return cached + fresh.
fn fetch_with_per_source_cache(
    src: &dyn FinancialSource,
    q: &Query,
    ctx: &Context,
    today: time::Date,
) -> Result<Vec<FinancialRow>, SiftError> {
    let source_tag = SourceTag::from_name(src.name()).unwrap_or(SourceTag::EastMoney);
    let mut cached: Vec<FinancialRow> = Vec::new();
    let mut miss_periods: Vec<Period> = Vec::new();

    if let Some(cache) = ctx.cache.as_ref() {
        for p in &q.periods {
            match cache.get(&q.symbol, q.statement, *p, q.scope, source_tag, today) {
                Some(rows) => cached.extend(rows),
                None => miss_periods.push(*p),
            }
        }
    } else {
        miss_periods.extend(q.periods.iter().copied());
    }

    if miss_periods.is_empty() {
        return Ok(cached);
    }

    let sub_query = Query {
        periods: miss_periods,
        ..q.clone()
    };
    let fresh = src.fetch(&sub_query, ctx)?;
    if let Some(cache) = ctx.cache.as_ref() {
        if let Err(e) = cache.put(&fresh, source_tag) {
            // Cache write failure is non-fatal: log and continue.
            eprintln!("[warn] {} cache write failed: {e}", src.name());
        }
    }
    cached.extend(fresh);
    Ok(cached)
}


fn today_utc() -> time::Date {
    time::OffsetDateTime::now_utc().date()
}

// ---------------------------------------------------------------------------
// Panic message extraction
// ---------------------------------------------------------------------------

/// Extract a human-readable message from a `catch_unwind` payload.
/// Falls back to a sentinel string for opaque payload types.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return s.to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        AuditStatus, FinancialRow, Period, PeriodType, Scope, SourceTag, Statement, Symbol,
    };
    use crate::domain::market::Market;
    use std::time::Duration;
    use time::{Date, Month};

    // ---- Mock source -------------------------------------------------

    type SupportsFn = dyn Fn(&Query) -> bool + Send + Sync;
    type FetchFn = dyn Fn(&Query) -> Result<Vec<FinancialRow>, SiftError> + Send + Sync;

    struct MockSource {
        name: &'static str,
        supports_fn: Box<SupportsFn>,
        fetch_fn: Box<FetchFn>,
    }

    impl MockSource {
        fn new(
            name: &'static str,
            supports: impl Fn(&Query) -> bool + Send + Sync + 'static,
            fetch: impl Fn(&Query) -> Result<Vec<FinancialRow>, SiftError> + Send + Sync + 'static,
        ) -> Self {
            Self {
                name,
                supports_fn: Box::new(supports),
                fetch_fn: Box::new(fetch),
            }
        }
    }

    impl FinancialSource for MockSource {
        fn name(&self) -> &'static str {
            self.name
        }
        fn supports(&self, q: &Query) -> bool {
            (self.supports_fn)(q)
        }
        fn fetch(&self, q: &Query, _ctx: &Context) -> Result<Vec<FinancialRow>, SiftError> {
            (self.fetch_fn)(q)
        }
    }

    // ---- Fixtures ----------------------------------------------------

    fn fixture_query() -> Query {
        Query {
            symbol: Symbol {
                code: "600519".into(),
                market: Market::CnA,
            },
            statement: Statement::Income,
            periods: vec![Period::Annual(2024)],
            scope: Scope::Consolidated,
        }
    }

    fn fixture_row(source: SourceTag, marker_code: &str) -> FinancialRow {
        FinancialRow {
            symbol: Symbol {
                code: marker_code.into(),
                market: Market::CnA,
            },
            name: "test".into(),
            period: Date::from_calendar_date(2024, Month::December, 31).unwrap(),
            period_type: PeriodType::Annual,
            statement: Statement::Income,
            scope: Scope::Consolidated,
            item: "营业总收入".into(),
            value: 100.0,
            unit: crate::domain::Unit::Raw,
            currency: "CNY".into(),
            publish_date: None,
            audit: AuditStatus::Unknown,
            source,
        }
    }

    fn run(sources: &[&dyn FinancialSource]) -> Result<Vec<FinancialRow>, SiftError> {
        dispatch_against(&fixture_query(), &Context::default(), sources)
    }

    // ---- Tests -------------------------------------------------------

    #[test]
    fn no_applicable_source_returns_specific_error() {
        let a = MockSource::new("a", |_| false, |_| panic!("a fetch must not be called"));
        let b = MockSource::new("b", |_| false, |_| panic!("b fetch must not be called"));
        let err = run(&[&a, &b]).unwrap_err();
        assert!(
            matches!(err, SiftError::NoApplicableSource(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn single_source_success_returns_its_rows() {
        let a = MockSource::new(
            "em",
            |_| true,
            |_| Ok(vec![fixture_row(SourceTag::EastMoney, "600519")]),
        );
        let rows = run(&[&a]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, SourceTag::EastMoney);
    }

    #[test]
    fn fast_success_wins_over_slow_success() {
        let fast = MockSource::new(
            "fast",
            |_| true,
            |_| Ok(vec![fixture_row(SourceTag::EastMoney, "FAST")]),
        );
        let slow = MockSource::new(
            "slow",
            |_| true,
            |_| {
                std::thread::sleep(Duration::from_millis(50));
                Ok(vec![fixture_row(SourceTag::Sina, "SLOW")])
            },
        );
        let rows = run(&[&fast, &slow]).unwrap();
        assert_eq!(rows[0].symbol.code, "FAST");
    }

    #[test]
    fn fast_success_wins_when_slow_would_error() {
        let fast = MockSource::new(
            "fast",
            |_| true,
            |_| Ok(vec![fixture_row(SourceTag::EastMoney, "FAST")]),
        );
        let slow_err = MockSource::new(
            "slow_err",
            |_| true,
            |_| {
                std::thread::sleep(Duration::from_millis(50));
                Err(SiftError::Network("late boom".into()))
            },
        );
        let rows = run(&[&fast, &slow_err]).unwrap();
        assert_eq!(rows[0].symbol.code, "FAST");
    }

    #[test]
    fn slow_success_wins_when_fast_errors() {
        let fast_err = MockSource::new(
            "fast_err",
            |_| true,
            |_| Err(SiftError::Network("immediate boom".into())),
        );
        let slow = MockSource::new(
            "slow",
            |_| true,
            |_| {
                std::thread::sleep(Duration::from_millis(50));
                Ok(vec![fixture_row(SourceTag::Sina, "SLOW")])
            },
        );
        let rows = run(&[&fast_err, &slow]).unwrap();
        assert_eq!(rows[0].symbol.code, "SLOW");
    }

    #[test]
    fn all_fail_returns_aggregated_failure_list() {
        let a = MockSource::new(
            "a",
            |_| true,
            |_| Err(SiftError::Network("a-err".into())),
        );
        let b = MockSource::new(
            "b",
            |_| true,
            |_| Err(SiftError::Network("b-err".into())),
        );
        let err = run(&[&a, &b]).unwrap_err();
        match err {
            SiftError::AllSourcesFailed(failures) => {
                assert_eq!(failures.len(), 2);
                let names: Vec<&str> = failures.iter().map(|(n, _)| n.as_str()).collect();
                assert!(names.contains(&"a"), "names: {names:?}");
                assert!(names.contains(&"b"), "names: {names:?}");
                // Each failure carries the originating message.
                let joined = failures
                    .iter()
                    .map(|(_, m)| m.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                assert!(joined.contains("a-err"), "joined: {joined}");
                assert!(joined.contains("b-err"), "joined: {joined}");
            }
            other => panic!("expected AllSourcesFailed, got {other:?}"),
        }
    }

    #[test]
    fn supports_false_prevents_fetch_invocation() {
        let a_ok = MockSource::new(
            "a",
            |_| true,
            |_| Ok(vec![fixture_row(SourceTag::EastMoney, "OK")]),
        );
        let b_skipped = MockSource::new(
            "b",
            |_| false,
            |_| panic!("b.fetch must not be called when supports() == false"),
        );
        let rows = run(&[&a_ok, &b_skipped]).unwrap();
        assert_eq!(rows[0].symbol.code, "OK");
    }

    #[test]
    fn fetch_panic_is_caught_and_other_source_wins() {
        let panicky = MockSource::new("p", |_| true, |_| panic!("kaboom"));
        let ok = MockSource::new(
            "ok",
            |_| true,
            |_| Ok(vec![fixture_row(SourceTag::Sina, "WIN")]),
        );
        let rows = run(&[&panicky, &ok]).unwrap();
        assert_eq!(rows[0].symbol.code, "WIN");
    }

    #[test]
    fn all_panic_returns_aggregated_failures_with_panicked_messages() {
        let a = MockSource::new("a", |_| true, |_| panic!("a-panic"));
        let b = MockSource::new("b", |_| true, |_| panic!("b-panic"));
        let err = run(&[&a, &b]).unwrap_err();
        match err {
            SiftError::AllSourcesFailed(failures) => {
                assert_eq!(failures.len(), 2);
                for (_, msg) in &failures {
                    assert!(msg.contains("panicked"), "msg: {msg}");
                }
            }
            other => panic!("expected AllSourcesFailed, got {other:?}"),
        }
    }

    #[test]
    fn empty_ok_is_still_a_success() {
        // Sources may legitimately return zero rows (e.g. a period has
        // no data); dispatch must treat that as success, not "try the
        // next source".
        let empty = MockSource::new("empty", |_| true, |_| Ok(Vec::new()));
        let other = MockSource::new(
            "other",
            |_| true,
            |_| Ok(vec![fixture_row(SourceTag::EastMoney, "OTHER")]),
        );
        let rows = run(&[&empty, &other]).unwrap();
        // We can't guarantee `empty` wins vs `other` on timing — both
        // are immediate Ok. Just verify dispatch returned without
        // running off to `AllSourcesFailed`.
        assert!(rows.is_empty() || rows[0].symbol.code == "OTHER");
    }

    /// `init()` irreversibly touches the global `OnceLock`. This test
    /// is the **only** one in the binary that calls `init()`; every
    /// other test uses `dispatch_against` to bypass the global so that
    /// parallel test execution stays deterministic.
    #[test]
    fn init_panics_on_second_call() {
        init(Vec::new());
        let r = std::panic::catch_unwind(|| init(Vec::new()));
        assert!(r.is_err(), "second init() must panic");
    }
}
