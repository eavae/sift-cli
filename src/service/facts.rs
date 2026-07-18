//! Service layer for the fact store — **all** the logic that sits
//! between the view (`commands/{sql,fact}`) and the pure-CRUD storage
//! ([`crate::store::FactStore`]).
//!
//! Responsibilities: parse/validate TSV batches and single-row inputs
//! into [`FactRow`]s (controlled-vocabulary checks, canonical symbol
//! form, defaults), then hand them to the store; forward read-only /
//! escape-hatch SQL. Commands never touch `crate::store` directly.

use time::{Date, Month};

use crate::app::AppContext;
use crate::domain::period::{Period, PeriodType};
use crate::domain::symbol::Symbol;
use crate::error::SiftError;
use crate::service::tsv::{self, col, FromTsvRow};
use crate::store::{FactKey, FactRow, QMode, Scope};
// Command-facing result types are surfaced through the service layer
// so `commands/` never names `crate::store` directly (three-layer
// discipline; see f6 README grep).
pub use crate::store::{BatchOutcome, SqlOutcome};

fn store(app: &AppContext) -> Result<&crate::store::FactStore, SiftError> {
    app.facts.as_ref().ok_or_else(|| {
        SiftError::Io("fact store unavailable (could not resolve ~/.sift/facts.duckdb)".into())
    })
}

/// A single fact expressed as strings, straight from CLI args. The
/// command layer fills this; parsing / validation happens in
/// [`row_from_input`] so `commands/` stays free of domain logic.
pub struct FactInput<'a> {
    pub symbol: &'a str,
    /// Period literal (`2024A` / `2024Q3` / `2024-09-30`).
    pub period: &'a str,
    pub key: &'a str,
    pub value: &'a str,
    pub source: &'a str,
    pub qmode: &'a str,
    pub scope: &'a str,
    pub currency: Option<&'a str>,
    pub publish_date: Option<&'a str>,
}

/// Identifies a fact to remove — same fields as [`FactInput`] minus
/// the value / currency / publish date.
pub struct FactRef<'a> {
    pub symbol: &'a str,
    pub period: &'a str,
    pub key: &'a str,
    pub source: &'a str,
    pub qmode: &'a str,
    pub scope: &'a str,
}

// ---- read paths -----------------------------------------------------------

pub fn query(app: &AppContext, sql: &str) -> Result<(Vec<String>, Vec<Vec<String>>), SiftError> {
    store(app)?.query(sql)
}

pub fn execute(app: &AppContext, sql: &str) -> Result<SqlOutcome, SiftError> {
    store(app)?.execute(sql)
}

// ---- write paths ----------------------------------------------------------

/// Ingest already-built rows (used by story-02/04 producers too).
pub fn ingest(app: &AppContext, rows: &[FactRow], atomic: bool) -> Result<BatchOutcome, SiftError> {
    store(app)?.upsert_facts(rows, atomic)
}

/// Parse a `#`-header TSV batch and ingest it. In atomic mode a single
/// parse failure aborts the whole batch (error names the line); in
/// skip mode parse failures are merged into [`BatchOutcome::skipped`].
pub fn ingest_tsv(app: &AppContext, input: &str, atomic: bool) -> Result<BatchOutcome, SiftError> {
    let (rows, parse_errs) = tsv::parse_tsv::<FactRow>(input);
    if atomic {
        if let Some((line, why)) = parse_errs.first() {
            return Err(SiftError::Parse(format!("line {line}: {why}")));
        }
    }
    let mut out = store(app)?.upsert_facts(&rows, atomic)?;
    if !atomic && !parse_errs.is_empty() {
        out.skipped.extend(parse_errs);
        out.skipped.sort_by_key(|(i, _)| *i);
    }
    Ok(out)
}

/// Ingest one fact from CLI args.
pub fn set_one(app: &AppContext, inp: &FactInput) -> Result<BatchOutcome, SiftError> {
    let row = row_from_input(inp)?;
    ingest(app, &[row], true)
}

/// Remove one fact by its key.
pub fn remove(app: &AppContext, r: &FactRef) -> Result<usize, SiftError> {
    let (fiscal_year, period_type) = split_period(r.period)?;
    store(app)?.delete_fact(&FactKey {
        symbol: canonical_symbol(r.symbol)?,
        fiscal_year,
        period_type,
        qmode: parse_qmode(r.qmode)?,
        scope: parse_scope(r.scope)?,
        raw_key: non_empty(r.key, "key")?.to_string(),
        source: non_empty(r.source, "source")?.to_string(),
    })
}

/// Build a [`FactRow`] from a single CLI input (period literal form).
pub fn row_from_input(inp: &FactInput) -> Result<FactRow, SiftError> {
    let (fiscal_year, period_type) = split_period(inp.period)?;
    Ok(FactRow {
        symbol: canonical_symbol(inp.symbol)?,
        fiscal_year,
        period_type,
        qmode: parse_qmode(inp.qmode)?,
        scope: parse_scope(inp.scope)?,
        raw_key: non_empty(inp.key, "key")?.to_string(),
        source: non_empty(inp.source, "source")?.to_string(),
        value: parse_value(inp.value)?,
        currency: inp.currency.map(|s| s.to_string()),
        publish_date: inp
            .publish_date
            .map(parse_iso_date)
            .transpose()?,
        name: None,
    })
}

// ---- TSV row shape (batch `fact set`) -------------------------------------

impl FromTsvRow for FactRow {
    fn from_fields(header: &[String], fields: &[&str]) -> Result<Self, String> {
        let symbol = col(header, fields, "symbol").ok_or("missing symbol")?;
        let fiscal_year = col(header, fields, "fiscal_year")
            .ok_or("missing fiscal_year")?
            .parse::<i32>()
            .map_err(|e| format!("bad fiscal_year: {e}"))?;
        let period_type = col(header, fields, "period_type")
            .ok_or("missing period_type")?
            .parse::<PeriodType>()
            .map_err(|()| "bad period_type (annual/h1/q1/q2/q3/q4)".to_string())?;
        let raw_key = col(header, fields, "raw_key")
            .filter(|s| !s.is_empty())
            .ok_or("missing raw_key")?;
        let value = col(header, fields, "value")
            .ok_or("missing value")?
            .parse::<f64>()
            .map_err(|e| format!("bad value: {e}"))?;
        let qmode = parse_qmode(col(header, fields, "qmode").unwrap_or("na"))
            .map_err(|e| e.to_string())?;
        let scope = parse_scope(col(header, fields, "scope").unwrap_or("na"))
            .map_err(|e| e.to_string())?;
        let source = col(header, fields, "source").unwrap_or("manual");
        let currency = col(header, fields, "currency").filter(|s| !s.is_empty());
        let publish_date = match col(header, fields, "publish_date").filter(|s| !s.is_empty()) {
            Some(s) => Some(parse_iso_date(s).map_err(|e| e.to_string())?),
            None => None,
        };
        Ok(FactRow {
            symbol: canonical_symbol(symbol).map_err(|e| e.to_string())?,
            fiscal_year,
            period_type,
            qmode,
            scope,
            raw_key: raw_key.to_string(),
            source: source.to_string(),
            value,
            currency: currency.map(|s| s.to_string()),
            publish_date,
            name: None,
        })
    }
}

// ---- shared parsing helpers ----------------------------------------------

/// Accept either the canonical `{code}.{MARKET}` form (MARKET ∈
/// CN-A/HK/US) or any [`Symbol`]-parseable form (`600519`, `sh600519`,
/// `00700.hk`, …), returning the canonical form. Index instruments are
/// rejected — the fact store is fundamentals-only.
fn canonical_symbol(s: &str) -> Result<String, SiftError> {
    let s = s.trim();
    if let Some((code, market)) = s.rsplit_once('.') {
        let m = market.to_ascii_uppercase();
        if matches!(m.as_str(), "CN-A" | "HK" | "US") && !code.is_empty()
            && code.chars().all(|c| c.is_ascii_digit())
        {
            return Ok(format!("{code}.{m}"));
        }
    }
    let sym = Symbol::parse(s)?;
    if sym.kind == crate::domain::market::InstrumentKind::Index {
        return Err(SiftError::Parse(format!(
            "{s:?} is an index; the fact store holds fundamentals only"
        )));
    }
    Ok(sym.display_symbol())
}

/// Period literal → `(fiscal_year, period_type)`. Rejects a
/// non-aligned custom date (facts use aligned period types only).
fn split_period(period: &str) -> Result<(i32, PeriodType), SiftError> {
    let p = Period::parse(period)?;
    let pt = p.period_type().ok_or_else(|| {
        SiftError::Parse(format!("period {period:?} is not an aligned report end"))
    })?;
    Ok((p.end_date().year(), pt))
}

fn parse_qmode(s: &str) -> Result<QMode, SiftError> {
    s.parse::<QMode>()
        .map_err(|()| SiftError::Parse(format!("bad qmode {s:?} (cumulative/single/point/na)")))
}

fn parse_scope(s: &str) -> Result<Scope, SiftError> {
    s.parse::<Scope>()
        .map_err(|()| SiftError::Parse(format!("bad scope {s:?} (consolidated/parent/na)")))
}

fn parse_value(s: &str) -> Result<f64, SiftError> {
    s.trim()
        .parse::<f64>()
        .map_err(|e| SiftError::Parse(format!("bad value {s:?}: {e}")))
}

fn non_empty<'a>(s: &'a str, what: &str) -> Result<&'a str, SiftError> {
    let t = s.trim();
    if t.is_empty() {
        return Err(SiftError::Parse(format!("empty {what}")));
    }
    Ok(t)
}

fn parse_iso_date(s: &str) -> Result<Date, SiftError> {
    let mut parts = s.trim().split('-');
    let bad = || SiftError::Parse(format!("bad date {s:?} (want YYYY-MM-DD)"));
    let y: i32 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let m: u8 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let d: u8 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    if parts.next().is_some() {
        return Err(bad());
    }
    let month = Month::try_from(m).map_err(|_| bad())?;
    Date::from_calendar_date(y, month, d).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_symbol_accepts_dotted_and_parseable_forms() {
        assert_eq!(canonical_symbol("600519.CN-A").unwrap(), "600519.CN-A");
        assert_eq!(canonical_symbol("600519.cn-a").unwrap(), "600519.CN-A");
        assert_eq!(canonical_symbol("600519").unwrap(), "600519.CN-A");
        assert_eq!(canonical_symbol("sh600519").unwrap(), "600519.CN-A");
        assert_eq!(canonical_symbol("00700.hk").unwrap(), "00700.HK");
    }

    #[test]
    fn canonical_symbol_rejects_index() {
        assert!(canonical_symbol("sh000001").is_err());
    }

    #[test]
    fn split_period_maps_literals() {
        assert_eq!(split_period("2024A").unwrap(), (2024, PeriodType::Annual));
        assert_eq!(split_period("2024Q3").unwrap(), (2024, PeriodType::Q3));
        assert_eq!(split_period("2024-09-30").unwrap(), (2024, PeriodType::Q3));
    }

    #[test]
    fn split_period_rejects_non_aligned() {
        assert!(split_period("2024-08-15").is_err());
    }

    #[test]
    fn from_fields_applies_defaults() {
        let header: Vec<String> = ["symbol", "fiscal_year", "period_type", "raw_key", "value"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let row =
            FactRow::from_fields(&header, &["600519.CN-A", "2024", "annual", "employee_comp", "1.5e9"])
                .unwrap();
        assert_eq!(row.qmode, QMode::Na);
        assert_eq!(row.scope, Scope::Na);
        assert_eq!(row.source, "manual");
        assert_eq!(row.value, 1.5e9);
        assert_eq!(row.raw_key, "employee_comp");
    }

    #[test]
    fn from_fields_rejects_bad_period_type() {
        let header: Vec<String> = ["symbol", "fiscal_year", "period_type", "raw_key", "value"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = FactRow::from_fields(&header, &["600519.CN-A", "2024", "ytd", "k", "1"])
            .unwrap_err();
        assert!(err.contains("period_type"), "{err}");
    }
}
