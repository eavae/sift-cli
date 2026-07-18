//! Report data-access coordinator.
//!
//! Owns the first-success-wins multi-source dispatch + per-source
//! cache coordination for `sift report`. `commands/report.rs` calls
//! into the public entry points here ([`dispatch_with_cache_named`],
//! [`list_periods_union`]); every report cache interaction goes through
//! the `financials_cache_*` adapter section at the bottom of this
//! file. Financial rows and announce metadata share one
//! [`crate::cache::record::RecordCache`].
//!
//! ## Layering
//!
//! `fetch::report` reads the source list straight off
//! [`crate::app::AppContext::sources`] — `main.rs` builds the list and
//! threads `&AppContext` down. The `OnceLock`-backed registry that
//! used to live in `sources::financial_source` is gone; tests inject a
//! source list by constructing their own `AppContext`.

use std::sync::mpsc;

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Iso8601;
use time::Date;

use crate::app::AppContext;
use crate::cache::record::{Kind as CacheKind, RecordCache};
use crate::cache::ttl;
use crate::domain::market::Market;
use crate::domain::{
    AuditStatus, FinancialRow, Period, PeriodType, Query, Scope, SourceTag, Statement, Symbol, Unit,
};
use crate::error::SiftError;
use crate::sources::financial_source::FinancialSource;

// ===========================================================================
// ReportContext + dispatch
// ===========================================================================

/// Report-specific ambient state: the cross-cutting [`AppContext`] plus
/// the registered source list. Constructed by `main::run_report` and
/// passed through `commands::report::run` down into the dispatch
/// functions. Borrowed everywhere — `app` and `sources` each live in
/// their own slot on `main`'s stack frame, so the wrapper just
/// bundles two references for ergonomic single-argument signatures.
pub struct ReportContext<'a> {
    pub app: &'a AppContext,
    pub sources: &'a [Box<dyn FinancialSource>],
}

/// First-success-wins dispatch across every applicable source in
/// `ctx.sources`. Each worker hits the per-source cache first and
/// only the missing periods go to HTTP. Returns the first source
/// that completes successfully; all-`Err` yields
/// [`SiftError::AllSourcesFailed`].
pub fn dispatch_with_cache(
    query: &Query,
    ctx: &ReportContext,
) -> Result<Vec<FinancialRow>, SiftError> {
    // The auto race only includes sources that opt in via
    // `auto_dispatch()`. sina opts out — its A-share item labels are
    // Chinese while EM's are the raw English column codes, so letting
    // both win nondeterministically would flip the field names run to
    // run. sina stays reachable through `--source sina`
    // (`dispatch_with_cache_named`), which ignores this filter.
    let refs: Vec<&dyn FinancialSource> = ctx
        .sources
        .iter()
        .filter(|b| b.auto_dispatch())
        .map(|b| b.as_ref())
        .collect();
    dispatch_inner(query, ctx.app, &refs)
}

/// Like [`dispatch_with_cache`] but restricted to a single named
/// source. `name = None` falls back to the standard
/// first-success-wins behavior. An unknown name surfaces as
/// `NoApplicableSource` listing the registered sources.
pub fn dispatch_with_cache_named(
    query: &Query,
    ctx: &ReportContext,
    name: Option<&str>,
) -> Result<Vec<FinancialRow>, SiftError> {
    let Some(want) = name else {
        return dispatch_with_cache(query, ctx);
    };
    let Some(src) = ctx.sources.iter().find(|s| s.name() == want) else {
        let known: Vec<&str> = ctx.sources.iter().map(|s| s.name()).collect();
        return Err(SiftError::NoApplicableSource(format!(
            "--source {want} not registered; known sources: {}",
            known.join(", ")
        )));
    };
    dispatch_inner(query, ctx.app, &[src.as_ref()])
}

/// Race a fixed slice of source references against the shared
/// `AppContext`; private because the two public entry points
/// construct that slice for it (either the full `ctx.sources` view
/// or the one-element pinned view).
fn dispatch_inner(
    query: &Query,
    app: &AppContext,
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
                    fetch_with_per_source_cache(src, query, app, today)
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

/// One source's cache-aware fetch: hit cache for each period, fetch
/// only the misses, write fresh results back, return cached + fresh.
fn fetch_with_per_source_cache(
    src: &dyn FinancialSource,
    q: &Query,
    app: &AppContext,
    today: Date,
) -> Result<Vec<FinancialRow>, SiftError> {
    let source_tag = SourceTag::from_name(src.name()).unwrap_or(SourceTag::EastMoney);
    let mut cached: Vec<FinancialRow> = Vec::new();
    let mut miss_periods: Vec<Period> = Vec::new();

    if let Some(cache) = app.records_cache.as_ref() {
        for p in &q.periods {
            match financials_cache_get(cache, &q.symbol, q.statement, *p, q.scope, source_tag, today) {
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
    let fresh = src.fetch(&sub_query, &app.http)?;
    if let Some(cache) = app.records_cache.as_ref() {
        financials_cache_put(cache, &fresh, source_tag, src.name());
    }
    cached.extend(fresh);
    Ok(cached)
}

fn today_utc() -> Date {
    time::OffsetDateTime::now_utc().date()
}

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

// ===========================================================================
// `list_periods_union` — used by `sift report periods <symbol>`
// ===========================================================================

/// Union the `list_periods` results of every applicable source for
/// `symbol`, optionally pinning to one source. Returns
/// `(period_end, period_type, source_name)` triples sorted
/// **newest-first** (tiebreak by source name ascending).
///
/// `pinned = Some("eastmoney")` walks only that source; `None`
/// walks every source whose `supports(probe)` returns true (probe
/// = Income + Consolidated, sufficient to gate sources that opt
/// out of e.g. parent scope).
///
/// Returns [`SiftError::NoApplicableSource`] if no source matched
/// (covering both "unknown `--source` name" and "no source supports
/// this symbol + default scope").
pub fn list_periods_union(
    symbol: &Symbol,
    ctx: &ReportContext,
    pinned: Option<&str>,
) -> Result<Vec<(Date, PeriodType, String)>, SiftError> {
    use std::collections::BTreeSet;

    let mut any_called = false;
    let mut union: BTreeSet<(Date, PeriodType, String)> = BTreeSet::new();
    for src in ctx.sources {
        match pinned {
            Some(want) if src.name() != want => continue,
            // Unpinned: honor the same auto-dispatch opt-out as the
            // statement race so `periods` reflects the default sources.
            None if !src.auto_dispatch() => continue,
            _ => {}
        }
        let probe = Query {
            symbol: symbol.clone(),
            statement: Statement::Income,
            periods: vec![],
            scope: Scope::Consolidated,
        };
        if !src.supports(&probe) {
            continue;
        }
        any_called = true;
        if let Ok(periods) = src.list_periods(symbol, &ctx.app.http) {
            for p in periods {
                let pt = p.period_type().unwrap_or(PeriodType::Annual);
                union.insert((p.end_date(), pt, src.name().to_string()));
            }
        }
    }
    if !any_called {
        return Err(SiftError::NoApplicableSource(format!(
            "no source supports {} for periods query",
            symbol.code
        )));
    }
    let mut items: Vec<(Date, PeriodType, String)> = union.into_iter().collect();
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));
    Ok(items)
}

// ===========================================================================
// `load_listing_names` — listing cache read-through for `sift report` rendering
// ===========================================================================

/// Read the cninfo listing cache and return the `code → 中文简称`
/// map. `sift report` uses this to back-fill the security short
/// name for rows where the source (sina) did not provide one — the
/// alternative would be leaving the rendered table with blanks in the
/// `name` column, since sina's lrb endpoint omits names.
///
/// Thin pass-through over [`crate::fetch::search::cached_names`];
/// exists so [`commands::report`] flows through `fetch::report::*`
/// for every piece of data access (commands must never reach into
/// another command's fetch module). Missing / empty cache returns
/// an empty map — render falls back to whatever `name` the source
/// already populated.
pub fn load_listing_names(ctx: &AppContext) -> std::collections::HashMap<String, String> {
    crate::fetch::search::cached_names(ctx)
}

// ===========================================================================
// Financials cache adapter
// ===========================================================================
//
// Report financials and announce metadata share one
// `cache_entries(key, body, …)` table behind `RecordCache`. This
// adapter is the only piece that knows report-specific semantics:
// how to build the composite key, how to serialize a period's worth
// of rows into the opaque body, and where to apply the three-bucket
// TTL.

/// JSON-friendly mirror of [`FinancialRow`] used as the cache body
/// format. Kept private to `fetch::report` so changes here don't
/// touch the public `--format json` shape (which `FinancialRow`
/// itself doesn't even derive `Serialize` for).
#[derive(Debug, Serialize, Deserialize)]
struct StoredFinancialRow {
    symbol_code: String,
    symbol_market: u8,
    name: String,
    period: String,
    period_type: String,
    statement: String,
    scope: String,
    item: String,
    value: f64,
    unit: String,
    currency: String,
    publish_date: Option<String>,
    audit: String,
    source: String,
}

impl StoredFinancialRow {
    fn from_row(r: &FinancialRow) -> Self {
        Self {
            symbol_code: r.symbol.code.clone(),
            symbol_market: r.symbol.market as u8,
            name: r.name.clone(),
            period: r.period.format(&Iso8601::DATE).unwrap_or_default(),
            period_type: r.period_type.as_str().into(),
            statement: r.statement.as_str().into(),
            scope: r.scope.as_str().into(),
            item: r.item.clone(),
            value: r.value,
            unit: r.unit.as_str().into(),
            currency: r.currency.clone(),
            publish_date: r
                .publish_date
                .map(|d| d.format(&Iso8601::DATE).unwrap_or_default()),
            audit: r.audit.as_str().into(),
            source: r.source.as_str().into(),
        }
    }

    fn into_row(self) -> FinancialRow {
        FinancialRow {
            symbol: Symbol {
                code: self.symbol_code,
                market: market_from_u8(self.symbol_market),
                kind: crate::domain::market::InstrumentKind::Equity,
            },
            name: self.name,
            period: parse_iso_date(&self.period).unwrap_or_else(epoch_date),
            period_type: self.period_type.parse().unwrap_or(PeriodType::Annual),
            statement: self.statement.parse().unwrap_or(Statement::Income),
            scope: self.scope.parse().unwrap_or(Scope::Consolidated),
            item: self.item,
            value: self.value,
            unit: self.unit.parse().unwrap_or(Unit::Raw),
            currency: self.currency,
            publish_date: self.publish_date.as_deref().and_then(parse_iso_date),
            audit: self.audit.parse().unwrap_or(AuditStatus::Unknown),
            source: SourceTag::from_name(&self.source).unwrap_or(SourceTag::EastMoney),
        }
    }
}

/// Scope segments used as the `RecordCache` key prefix for a financials row.
/// `id` is the period's ISO end-date; the rest go into `scope` so the
/// blake3 hash separates entries cleanly across symbols / statements /
/// scopes / sources. The string slices borrow from the inputs — call
/// sites build a `Vec<&str>` and pass `.as_slice()`.
fn financials_scope(
    sym: &Symbol,
    stmt: Statement,
    scope: Scope,
    source: SourceTag,
) -> [&str; 5] {
    [
        sym.code.as_str(),
        sym.market.as_lower(),
        stmt.as_str(),
        scope.as_str(),
        source.as_str(),
    ]
}

/// Look up one cached `(sym, stmt, period, scope, source)` group.
/// Returns `None` on cache miss, on a row that's stale per
/// [`ttl::bucket_for`], on a missing `created_at`, or on a body that
/// fails to deserialize — every "not a usable hit" path collapses to
/// the same outcome, so the caller (miss → fetch) is unaffected by
/// which leg of the chain bailed.
fn financials_cache_get(
    cache: &RecordCache,
    sym: &Symbol,
    stmt: Statement,
    period: Period,
    scope: Scope,
    source: SourceTag,
    today: Date,
) -> Option<Vec<FinancialRow>> {
    let scope_parts = financials_scope(sym, stmt, scope, source);
    let id = period.end_date().format(&Iso8601::DATE).ok()?;
    let entry = cache.get(CacheKind::Financials, &scope_parts, &id)?;
    let created_at = entry.created_at?;
    let bucket = ttl::bucket_for(period.end_date(), today);
    if !ttl::is_fresh(created_at, bucket) {
        return None;
    }
    let stored: Vec<StoredFinancialRow> = serde_json::from_slice(&entry.body).ok()?;
    // Item labels are stored (and returned) verbatim — no dictionary
    // pass. The label a row was written with is the raw upstream label.
    Some(stored.into_iter().map(StoredFinancialRow::into_row).collect())
}

/// Upsert one fetch's rows into the cache. Groups the input by period
/// (the only axis that varies within a single source.fetch response —
/// symbol / statement / scope / source are fixed per dispatch call)
/// and batches the writes through one `put_many` so the DuckDB lock
/// is acquired once for the whole period set. Cache write failures
/// only log to stderr — they must never abort the user-facing fetch.
fn financials_cache_put(cache: &RecordCache, rows: &[FinancialRow], source: SourceTag, src_name: &str) {
    if rows.is_empty() {
        return;
    }
    use std::collections::BTreeMap;
    // The fixed components come from the first row; the dispatcher
    // guarantees a single (sym, stmt, scope) per call. Defensively
    // skip rows that disagree (they'd land in the wrong cache bucket).
    let head = &rows[0];
    let scope_parts = financials_scope(&head.symbol, head.statement, head.scope, source);

    // Group by period; each group becomes one `put_many` item.
    let mut by_period: BTreeMap<String, Vec<&FinancialRow>> = BTreeMap::new();
    for r in rows {
        if r.symbol != head.symbol
            || r.statement != head.statement
            || r.scope != head.scope
        {
            eprintln!(
                "[warn] {src_name} cache write: skipping row that disagrees on \
                 (symbol/statement/scope) with batch head"
            );
            continue;
        }
        let key = r.period.format(&Iso8601::DATE).unwrap_or_default();
        by_period.entry(key).or_default().push(r);
    }

    let items: Vec<(String, Vec<u8>)> = by_period
        .into_iter()
        .filter_map(|(period_iso, group)| {
            let stored: Vec<StoredFinancialRow> =
                group.iter().map(|r| StoredFinancialRow::from_row(r)).collect();
            match serde_json::to_vec(&stored) {
                Ok(body) => Some((period_iso, body)),
                Err(e) => {
                    eprintln!("[warn] {src_name} cache write: serialize: {e}");
                    None
                }
            }
        })
        .collect();

    cache.put_many(CacheKind::Financials, &scope_parts, items);
}

fn market_from_u8(b: u8) -> Market {
    match b {
        0 => Market::CnA,
        1 => Market::Hk,
        2 => Market::Us,
        _ => Market::CnA,
    }
}

fn parse_iso_date(s: &str) -> Option<Date> {
    let mut parts = s.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    let d: u8 = parts.next()?.parse().ok()?;
    Date::from_calendar_date(y, time::Month::try_from(m).ok()?, d).ok()
}

fn epoch_date() -> Date {
    Date::from_calendar_date(1970, time::Month::January, 1).unwrap()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::Market;
    use crate::domain::{AuditStatus, Scope, Statement};
    use crate::http::HttpClient;
    use std::time::Duration;
    use time::Month;

    // ---- Mock source -------------------------------------------------

    type SupportsFn = dyn Fn(&Query) -> bool + Send + Sync;
    type FetchFn = dyn Fn(&Query) -> Result<Vec<FinancialRow>, SiftError> + Send + Sync;
    type PeriodsFn = dyn Fn() -> Result<Vec<Period>, SiftError> + Send + Sync;

    struct MockSource {
        name: &'static str,
        supports_fn: Box<SupportsFn>,
        fetch_fn: Box<FetchFn>,
        periods_fn: Option<Box<PeriodsFn>>,
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
                periods_fn: None,
            }
        }

        fn with_periods(
            mut self,
            periods: impl Fn() -> Result<Vec<Period>, SiftError> + Send + Sync + 'static,
        ) -> Self {
            self.periods_fn = Some(Box::new(periods));
            self
        }
    }

    impl FinancialSource for MockSource {
        fn name(&self) -> &'static str {
            self.name
        }
        fn supports(&self, q: &Query) -> bool {
            (self.supports_fn)(q)
        }
        fn fetch(&self, q: &Query, _http: &HttpClient) -> Result<Vec<FinancialRow>, SiftError> {
            (self.fetch_fn)(q)
        }
        fn list_periods(
            &self,
            _symbol: &Symbol,
            _http: &HttpClient,
        ) -> Result<Vec<Period>, SiftError> {
            match &self.periods_fn {
                Some(f) => f(),
                None => Ok(Vec::new()),
            }
        }
    }

    // ---- Fixtures ----------------------------------------------------

    fn fixture_query() -> Query {
        Query {
            symbol: Symbol {
                code: "600519".into(),
                market: Market::CnA,
                kind: crate::domain::market::InstrumentKind::Equity,
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
                kind: crate::domain::market::InstrumentKind::Equity,
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

    /// Build the no-cache `AppContext` shared by every dispatch test.
    /// `records_cache = None` skips the cache pre-filter; the
    /// `financials_cache_*` adapter tests build their own ctx.
    fn empty_app() -> AppContext {
        AppContext::default()
    }

    fn run(sources: Vec<Box<dyn FinancialSource>>) -> Result<Vec<FinancialRow>, SiftError> {
        let app = empty_app();
        let ctx = ReportContext {
            app: &app,
            sources: &sources,
        };
        dispatch_with_cache(&fixture_query(), &ctx)
    }

    // ---- dispatch tests ----------------------------------------------

    #[test]
    fn no_applicable_source_returns_specific_error() {
        let a = MockSource::new("a", |_| false, |_| panic!("a fetch must not be called"));
        let b = MockSource::new("b", |_| false, |_| panic!("b fetch must not be called"));
        let err = run(vec![Box::new(a), Box::new(b)]).unwrap_err();
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
        let rows = run(vec![Box::new(a)]).unwrap();
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
        let rows = run(vec![Box::new(fast), Box::new(slow)]).unwrap();
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
        let rows = run(vec![Box::new(fast), Box::new(slow_err)]).unwrap();
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
        let rows = run(vec![Box::new(fast_err), Box::new(slow)]).unwrap();
        assert_eq!(rows[0].symbol.code, "SLOW");
    }

    #[test]
    fn all_fail_returns_aggregated_failure_list() {
        let a = MockSource::new("a", |_| true, |_| Err(SiftError::Network("a-err".into())));
        let b = MockSource::new("b", |_| true, |_| Err(SiftError::Network("b-err".into())));
        let err = run(vec![Box::new(a), Box::new(b)]).unwrap_err();
        match err {
            SiftError::AllSourcesFailed(failures) => {
                assert_eq!(failures.len(), 2);
                let names: Vec<&str> = failures.iter().map(|(n, _)| n.as_str()).collect();
                assert!(names.contains(&"a"), "names: {names:?}");
                assert!(names.contains(&"b"), "names: {names:?}");
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
        let rows = run(vec![Box::new(a_ok), Box::new(b_skipped)]).unwrap();
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
        let rows = run(vec![Box::new(panicky), Box::new(ok)]).unwrap();
        assert_eq!(rows[0].symbol.code, "WIN");
    }

    #[test]
    fn all_panic_returns_aggregated_failures_with_panicked_messages() {
        let a = MockSource::new("a", |_| true, |_| panic!("a-panic"));
        let b = MockSource::new("b", |_| true, |_| panic!("b-panic"));
        let err = run(vec![Box::new(a), Box::new(b)]).unwrap_err();
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
        let rows = run(vec![Box::new(empty), Box::new(other)]).unwrap();
        // We can't guarantee `empty` wins vs `other` on timing — both
        // are immediate Ok. Just verify dispatch returned without
        // running off to `AllSourcesFailed`.
        assert!(rows.is_empty() || rows[0].symbol.code == "OTHER");
    }

    // ---- list_periods_union tests -----------------------------------

    fn maotai() -> Symbol {
        Symbol {
            code: "600519".into(),
            market: Market::CnA,
            kind: crate::domain::market::InstrumentKind::Equity,
        }
    }

    #[test]
    fn list_periods_union_unions_two_sources_sorted_newest_first() {
        let em = MockSource::new("em", |_| true, |_| Ok(vec![]))
            .with_periods(|| Ok(vec![Period::Annual(2023), Period::Annual(2024)]));
        let sina = MockSource::new("sina", |_| true, |_| Ok(vec![]))
            .with_periods(|| Ok(vec![Period::Annual(2024), Period::Annual(2022)]));

        let app = empty_app();
        let srcs: Vec<Box<dyn FinancialSource>> = vec![Box::new(em), Box::new(sina)];
        let ctx = ReportContext { app: &app, sources: &srcs };
        let got = list_periods_union(&maotai(), &ctx, None).unwrap();
        // 4 rows: (2024, em), (2024, sina), (2023, em), (2022, sina);
        // newest-first by date, tiebreak source name asc.
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].0.year(), 2024);
        assert_eq!(got[0].2, "em");
        assert_eq!(got[1].0.year(), 2024);
        assert_eq!(got[1].2, "sina");
        assert_eq!(got[2].0.year(), 2023);
        assert_eq!(got[2].2, "em");
        assert_eq!(got[3].0.year(), 2022);
        assert_eq!(got[3].2, "sina");
    }

    #[test]
    fn list_periods_union_pinned_uses_only_named_source() {
        let em = MockSource::new("em", |_| true, |_| Ok(vec![]))
            .with_periods(|| Ok(vec![Period::Annual(2024)]));
        // sina's periods callback panics — proves it isn't invoked.
        let sina = MockSource::new("sina", |_| true, |_| Ok(vec![]))
            .with_periods(|| panic!("sina.list_periods must not be called when pinned=em"));

        let app = empty_app();
        let srcs: Vec<Box<dyn FinancialSource>> = vec![Box::new(em), Box::new(sina)];
        let ctx = ReportContext { app: &app, sources: &srcs };
        let got = list_periods_union(&maotai(), &ctx, Some("em")).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].2, "em");
    }

    #[test]
    fn list_periods_union_no_applicable_source_when_none_supports() {
        let em = MockSource::new("em", |_| false, |_| Ok(vec![]))
            .with_periods(|| panic!("supports=false must short-circuit"));
        let sina = MockSource::new("sina", |_| false, |_| Ok(vec![]));
        let app = empty_app();
        let srcs: Vec<Box<dyn FinancialSource>> = vec![Box::new(em), Box::new(sina)];
        let ctx = ReportContext { app: &app, sources: &srcs };
        let err = list_periods_union(&maotai(), &ctx, None).unwrap_err();
        assert!(matches!(err, SiftError::NoApplicableSource(_)));
    }

    #[test]
    fn list_periods_union_pinned_unknown_name_returns_no_applicable_source() {
        let em = MockSource::new("em", |_| true, |_| Ok(vec![]))
            .with_periods(|| Ok(vec![Period::Annual(2024)]));
        let app = empty_app();
        let srcs: Vec<Box<dyn FinancialSource>> = vec![Box::new(em)];
        let ctx = ReportContext { app: &app, sources: &srcs };
        let err = list_periods_union(&maotai(), &ctx, Some("nope")).unwrap_err();
        assert!(matches!(err, SiftError::NoApplicableSource(_)));
    }

    // ---- financials cache adapter tests ------------------------------
    //
    // These verify the `financials_cache_*` adapter that wraps the
    // generic `RecordCache` with (sym, stmt, period, scope, source)
    // scoping + TTL + dict renormalize.

    use tempfile::TempDir;
    use time::OffsetDateTime;

    fn financials_cache(tmp: &TempDir) -> RecordCache {
        RecordCache::open_at(&tmp.path().join("records.duckdb")).unwrap()
    }

    fn fc_row(period_end: Date, item: &str, value: f64, source: SourceTag) -> FinancialRow {
        FinancialRow {
            symbol: Symbol {
                code: "600519".into(),
                market: Market::CnA,
                kind: crate::domain::market::InstrumentKind::Equity,
            },
            name: "贵州茅台".into(),
            period: period_end,
            period_type: PeriodType::from_date(period_end).unwrap_or(PeriodType::Annual),
            statement: Statement::Income,
            scope: Scope::Consolidated,
            item: item.into(),
            value,
            unit: crate::domain::Unit::Raw,
            currency: "CNY".into(),
            publish_date: None,
            audit: AuditStatus::Unknown,
            source,
        }
    }

    fn date(y: i32, m: u8, d: u8) -> Date {
        Date::from_calendar_date(y, time::Month::try_from(m).unwrap(), d).unwrap()
    }

    #[test]
    fn financials_cache_put_get_round_trip_returns_identical_rows() {
        let tmp = TempDir::new().unwrap();
        let cache = financials_cache(&tmp);
        let period = date(2025, 12, 31);
        let rows = vec![
            fc_row(period, "营业总收入", 172054171891.0, SourceTag::EastMoney),
            fc_row(period, "归母净利润", 82320067102.0, SourceTag::EastMoney),
        ];
        financials_cache_put(&cache, &rows, SourceTag::EastMoney, "eastmoney");
        let got = financials_cache_get(
            &cache,
            &rows[0].symbol,
            Statement::Income,
            Period::Annual(2025),
            Scope::Consolidated,
            SourceTag::EastMoney,
            date(2026, 5, 20),
        )
        .unwrap();
        assert_eq!(got.len(), 2);
        let by_item: std::collections::HashMap<_, _> =
            got.iter().map(|r| (r.item.clone(), r.value)).collect();
        assert_eq!(by_item["营业总收入"], 172054171891.0);
        assert_eq!(by_item["归母净利润"], 82320067102.0);
    }

    #[test]
    fn financials_cache_stale_recent_bucket_returns_none() {
        let tmp = TempDir::new().unwrap();
        let cache = financials_cache(&tmp);
        let period = date(2026, 3, 31);
        let rows = vec![fc_row(period, "营业总收入", 1.0, SourceTag::EastMoney)];
        financials_cache_put(&cache, &rows, SourceTag::EastMoney, "eastmoney");
        // Force the row's created_at to 25h ago — past the 24h Recent-bucket TTL.
        let scope = financials_scope(
            &rows[0].symbol,
            Statement::Income,
            Scope::Consolidated,
            SourceTag::EastMoney,
        );
        let id = Period::Q1(2026).end_date().format(&Iso8601::DATE).unwrap();
        cache.force_created_at(
            CacheKind::Financials,
            &scope,
            &id,
            OffsetDateTime::now_utc() - time::Duration::hours(25),
        );
        let got = financials_cache_get(
            &cache,
            &rows[0].symbol,
            Statement::Income,
            Period::Q1(2026),
            Scope::Consolidated,
            SourceTag::EastMoney,
            date(2026, 5, 20),
        );
        assert!(got.is_none(), "stale Recent-bucket row must miss");
    }

    #[test]
    fn financials_cache_old_bucket_is_always_fresh() {
        let tmp = TempDir::new().unwrap();
        let cache = financials_cache(&tmp);
        let period = date(2020, 12, 31);
        let rows = vec![fc_row(period, "营业总收入", 1.0, SourceTag::EastMoney)];
        financials_cache_put(&cache, &rows, SourceTag::EastMoney, "eastmoney");
        let scope = financials_scope(
            &rows[0].symbol,
            Statement::Income,
            Scope::Consolidated,
            SourceTag::EastMoney,
        );
        let id = Period::Annual(2020).end_date().format(&Iso8601::DATE).unwrap();
        cache.force_created_at(
            CacheKind::Financials,
            &scope,
            &id,
            OffsetDateTime::now_utc() - time::Duration::days(365 * 5),
        );
        let got = financials_cache_get(
            &cache,
            &rows[0].symbol,
            Statement::Income,
            Period::Annual(2020),
            Scope::Consolidated,
            SourceTag::EastMoney,
            date(2026, 5, 20),
        );
        assert!(got.is_some(), "Old-bucket row is permanently fresh");
    }

    #[test]
    fn financials_cache_upsert_overwrites_in_place() {
        let tmp = TempDir::new().unwrap();
        let cache = financials_cache(&tmp);
        let period = date(2025, 12, 31);
        let v1 = vec![fc_row(period, "营业总收入", 1.0, SourceTag::EastMoney)];
        let v2 = vec![fc_row(period, "营业总收入", 2.0, SourceTag::EastMoney)];
        financials_cache_put(&cache, &v1, SourceTag::EastMoney, "eastmoney");
        financials_cache_put(&cache, &v2, SourceTag::EastMoney, "eastmoney");
        let got = financials_cache_get(
            &cache,
            &v1[0].symbol,
            Statement::Income,
            Period::Annual(2025),
            Scope::Consolidated,
            SourceTag::EastMoney,
            date(2026, 5, 20),
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].value, 2.0);
    }

    #[test]
    fn financials_cache_different_sources_do_not_share() {
        let tmp = TempDir::new().unwrap();
        let cache = financials_cache(&tmp);
        let period = date(2025, 12, 31);
        let em = vec![fc_row(period, "营业总收入", 1.0, SourceTag::EastMoney)];
        let sina = vec![fc_row(period, "营业总收入", 2.0, SourceTag::Sina)];
        financials_cache_put(&cache, &em, SourceTag::EastMoney, "eastmoney");
        financials_cache_put(&cache, &sina, SourceTag::Sina, "sina");
        let em_got = financials_cache_get(
            &cache,
            &em[0].symbol,
            Statement::Income,
            Period::Annual(2025),
            Scope::Consolidated,
            SourceTag::EastMoney,
            date(2026, 5, 20),
        )
        .unwrap();
        let sina_got = financials_cache_get(
            &cache,
            &sina[0].symbol,
            Statement::Income,
            Period::Annual(2025),
            Scope::Consolidated,
            SourceTag::Sina,
            date(2026, 5, 20),
        )
        .unwrap();
        assert_eq!(em_got[0].value, 1.0);
        assert_eq!(em_got[0].source, SourceTag::EastMoney);
        assert_eq!(sina_got[0].value, 2.0);
        assert_eq!(sina_got[0].source, SourceTag::Sina);
    }

    #[test]
    fn financials_cache_preserves_item_label_verbatim_on_read() {
        // Items round-trip through the cache unchanged — there is no
        // dictionary pass, so whatever raw upstream label was written
        // comes back exactly.
        let tmp = TempDir::new().unwrap();
        let cache = financials_cache(&tmp);
        let period = date(2025, 12, 31);
        let row = fc_row(period, "OPERATE_INCOME", 100.0, SourceTag::EastMoney);
        financials_cache_put(&cache, std::slice::from_ref(&row), SourceTag::EastMoney, "eastmoney");
        let got = financials_cache_get(
            &cache,
            &row.symbol,
            Statement::Income,
            Period::Annual(2025),
            Scope::Consolidated,
            SourceTag::EastMoney,
            date(2026, 5, 20),
        )
        .unwrap();
        // Items round-trip through the cache verbatim — no dictionary
        // pass on read, so the raw upstream label is preserved.
        assert_eq!(got[0].item, "OPERATE_INCOME");
    }
}
