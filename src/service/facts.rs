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
use crate::domain::financial_row::{FinancialRow, Scope as DomainScope, Statement};
use crate::domain::period::{Period, PeriodType};
use crate::domain::symbol::Symbol;
use crate::error::SiftError;
use crate::service::store;
use crate::service::tsv::{self, col, FromTsvRow};
use crate::store::{FactKey, FactRow, QMode, Scope, SymbolName};
// Command-facing result types are surfaced through the service layer
// so `commands/` never names `crate::store` directly (three-layer
// discipline; see f6 README grep).
pub use crate::store::{BatchOutcome, SqlOutcome};

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

/// Ingest already-built rows with their resolved symbol dimension.
pub fn ingest(
    app: &AppContext,
    syms: &[SymbolName],
    rows: &[FactRow],
    atomic: bool,
) -> Result<BatchOutcome, SiftError> {
    store(app)?.upsert_facts(syms, rows, atomic)
}

/// Map one `report` row to a [`FactRow`]. `qmode` is derived from the
/// statement + single-quarter flag (income/cashflow →
/// single/cumulative; balance → point; indicator → na); `scope`
/// follows the row. The caller must pass **raw** rows (before
/// `apply_unit` scaling) — the fact store holds raw values only.
pub fn fact_row_from(r: &FinancialRow, single: bool) -> FactRow {
    let qmode = match r.statement {
        Statement::Income | Statement::Cashflow => {
            if single {
                QMode::Single
            } else {
                QMode::Cumulative
            }
        }
        Statement::Balance => QMode::Point,
        Statement::Indicator => QMode::Na,
    };
    let scope = match r.scope {
        DomainScope::Consolidated => Scope::Consolidated,
        DomainScope::Parent => Scope::Parent,
    };
    FactRow {
        symbol: r.symbol.display_symbol(),
        fiscal_year: r.period.year(),
        period_type: r.period_type,
        qmode,
        scope,
        raw_key: r.item.clone(),
        source: r.source.as_str().to_string(),
        value: r.value,
        currency: non_blank(&r.currency),
        publish_date: r.publish_date,
    }
}

/// Collect the distinct `(symbol, name)` dimension entries a producer
/// already knows (`report` / `market` carry the name on every row).
/// Rows with a blank name contribute no entry — the symbol still gets
/// a bare stub from the store only if it is otherwise absent.
fn syms_from<'a>(pairs: impl Iterator<Item = (String, &'a str)>) -> Vec<SymbolName> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (symbol, name) in pairs {
        let name = name.trim();
        if name.is_empty() || !seen.insert(symbol.clone()) {
            continue;
        }
        out.push(SymbolName {
            symbol,
            name: name.to_string(),
        });
    }
    out
}

/// Best-effort ingest of a statement's raw rows. `Ok(None)` means the
/// fact store was unavailable (caller warns and moves on); `Ok(Some)`
/// is a completed write; `Err` is a store failure the caller downgrades
/// to a warning — ingest never aborts the user-facing report.
pub fn ingest_statement(
    app: &AppContext,
    rows: &[FinancialRow],
    single: bool,
) -> Result<Option<BatchOutcome>, SiftError> {
    let Some(st) = app.facts.as_ref() else {
        return Ok(None);
    };
    let syms = syms_from(rows.iter().map(|r| (r.symbol.display_symbol(), r.name.as_str())));
    let facts: Vec<FactRow> = rows.iter().map(|r| fact_row_from(r, single)).collect();
    st.upsert_facts(&syms, &facts, true).map(Some)
}

fn non_blank(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Map a whole-market snapshot to fact rows. Amount columns
/// ([`eastmoney_screen::AMOUNT_COLS`]) become `qmode = cumulative`;
/// everything else (ratios / per-share / growth) becomes `qmode = na`.
/// scope is always consolidated; source is `screen`.
pub fn market_facts(
    rows: &[crate::sources::eastmoney_screen::MarketRow],
    fiscal_year: i32,
    period_type: PeriodType,
) -> Vec<FactRow> {
    use crate::sources::eastmoney_screen::AMOUNT_COLS;
    let mut out = Vec::new();
    for r in rows {
        let symbol = format!("{}.CN-A", r.code);
        let publish_date = r.notice_date.as_deref().and_then(|s| parse_iso_date(s).ok());
        for (key, value) in &r.metrics {
            let qmode = if AMOUNT_COLS.contains(&key.as_str()) {
                QMode::Cumulative
            } else {
                QMode::Na
            };
            out.push(FactRow {
                symbol: symbol.clone(),
                fiscal_year,
                period_type,
                qmode,
                scope: Scope::Consolidated,
                raw_key: key.clone(),
                source: "screen".into(),
                value: *value,
                currency: None,
                publish_date,
            });
        }
    }
    out
}

/// Best-effort whole-market ingest (source=screen). Same contract as
/// [`ingest_statement`]: `Ok(None)` when the store is unavailable.
pub fn ingest_market(
    app: &AppContext,
    rows: &[crate::sources::eastmoney_screen::MarketRow],
    fiscal_year: i32,
    period_type: PeriodType,
) -> Result<Option<BatchOutcome>, SiftError> {
    let Some(st) = app.facts.as_ref() else {
        return Ok(None);
    };
    let syms = syms_from(rows.iter().map(|r| (format!("{}.CN-A", r.code), r.name.as_str())));
    let facts = market_facts(rows, fiscal_year, period_type);
    let written = st.upsert_facts_bulk(&syms, &facts)?;
    Ok(Some(BatchOutcome {
        written,
        skipped: Vec::new(),
    }))
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
    // Resolve + validate every distinct symbol against the listing.
    // In atomic mode an unresolvable symbol aborts the batch; in skip
    // mode its rows are dropped and recorded in `skipped`.
    let (syms, bad) = resolve_row_symbols(app, &rows);
    if atomic {
        if let Some((_symbol, why)) = bad.iter().next() {
            // `why` already names the offending code.
            return Err(SiftError::Parse(why.clone()));
        }
    }
    let kept: Vec<FactRow> = rows
        .iter()
        .filter(|r| !bad.contains_key(&r.symbol))
        .cloned()
        .collect();
    let mut out = store(app)?.upsert_facts(&syms, &kept, atomic)?;
    if !atomic {
        for (i, r) in rows.iter().enumerate() {
            if let Some(why) = bad.get(&r.symbol) {
                out.skipped.push((i + 1, why.clone()));
            }
        }
        out.skipped.extend(parse_errs);
        out.skipped.sort_by_key(|(i, _)| *i);
    }
    Ok(out)
}

/// Ingest one fact from CLI args. The symbol is resolved + validated
/// against the cninfo listing (rejecting anything not listed) — this
/// is where the real `symbols.name` comes from for manual entries.
pub fn set_one(app: &AppContext, inp: &FactInput) -> Result<BatchOutcome, SiftError> {
    let row = row_from_input(inp)?;
    let sym = resolve_symbol(app, &row.symbol)?;
    ingest(app, &[sym], &[row], true)
}

/// Resolve + validate one canonical symbol against the cninfo listing,
/// returning the real name. Strict: a code absent from the listing is
/// rejected ([`SiftError::MissingOrgId`]). The market suffix must match
/// what the listing says (`00700.CN-A` → rejected: 00700 is HK).
fn resolve_symbol(app: &AppContext, canonical: &str) -> Result<SymbolName, SiftError> {
    let (code, suffix) = canonical
        .rsplit_once('.')
        .ok_or_else(|| SiftError::Parse(format!("symbol {canonical:?} lacks a .MARKET suffix")))?;
    let resolved = crate::fetch::search::resolve_org_id(app, code)?;
    let want = resolved.market.as_upper();
    if suffix != want {
        return Err(SiftError::Parse(format!(
            "symbol {canonical:?}: code {code} is a {want} listing, not {suffix}"
        )));
    }
    let name = crate::fetch::search::cached_names(app)
        .remove(code)
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| canonical.to_string());
    Ok(SymbolName {
        symbol: canonical.to_string(),
        name,
    })
}

/// Resolve every distinct symbol in `rows`. Returns the resolved
/// dimension entries plus a `symbol → reason` map for the ones that
/// failed validation (never errors itself — the caller decides
/// abort-vs-skip). Each distinct code is resolved once; the first
/// cache-cold lookup warms the whole listing.
fn resolve_row_symbols(
    app: &AppContext,
    rows: &[FactRow],
) -> (Vec<SymbolName>, std::collections::HashMap<String, String>) {
    let mut syms = Vec::new();
    let mut bad = std::collections::HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for r in rows {
        if !seen.insert(r.symbol.clone()) {
            continue;
        }
        match resolve_symbol(app, &r.symbol) {
            Ok(s) => syms.push(s),
            Err(e) => {
                bad.insert(r.symbol.clone(), format!("{e}"));
            }
        }
    }
    (syms, bad)
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
    let qmode = parse_qmode(inp.qmode)?;
    check_qmode_period(qmode, period_type).map_err(SiftError::Parse)?;
    Ok(FactRow {
        symbol: canonical_symbol(inp.symbol)?,
        fiscal_year,
        period_type,
        qmode,
        scope: parse_scope(inp.scope)?,
        raw_key: non_empty(inp.key, "key")?.to_string(),
        source: non_empty(inp.source, "source")?.to_string(),
        value: parse_value(inp.value)?,
        currency: inp.currency.map(|s| s.to_string()),
        publish_date: inp
            .publish_date
            .map(parse_iso_date)
            .transpose()?,
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
        check_qmode_period(qmode, period_type)?;
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

/// Service-side mirror of the facts cross-column CHECK: `single` ⇒
/// quarterly (q1/q2/q3/q4); every other qmode ⇒ annual/h1/q1/q3 (Q2/Q4
/// have no cumulative/point form). Pre-checked here so an illegal
/// combination gets a friendly message instead of DuckDB's raw
/// `Constraint Error` text; the DuckDB CHECK stays as the backstop.
/// Returns the reason string (callers wrap it: `SiftError::Parse` for
/// the single-row form, the TSV parse-error channel for batches).
fn check_qmode_period(qmode: QMode, pt: PeriodType) -> Result<(), String> {
    let ok = if let QMode::Single = qmode {
        matches!(
            pt,
            PeriodType::Q1 | PeriodType::Q2 | PeriodType::Q3 | PeriodType::Q4
        )
    } else {
        matches!(
            pt,
            PeriodType::Annual | PeriodType::H1 | PeriodType::Q1 | PeriodType::Q3
        )
    };
    if ok {
        return Ok(());
    }
    Err(if let QMode::Single = qmode {
        format!(
            "qmode=single requires a quarterly period_type (q1/q2/q3/q4); got {}",
            pt.as_str()
        )
    } else {
        format!(
            "qmode={} requires period_type annual/h1/q1/q3 (not q2/q4); got {}",
            qmode.as_str(),
            pt.as_str()
        )
    })
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
    fn syms_from_dedups_and_skips_blank_names() {
        let pairs = [
            ("600519.CN-A".to_string(), "贵州茅台"),
            ("600519.CN-A".to_string(), "贵州茅台"), // dup symbol → one entry
            ("000001.CN-A".to_string(), "  "),       // blank name → skipped
            ("00700.HK".to_string(), "腾讯控股"),
        ];
        let syms = syms_from(pairs.into_iter());
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].symbol, "600519.CN-A");
        assert_eq!(syms[0].name, "贵州茅台");
        assert_eq!(syms[1].symbol, "00700.HK");
    }

    #[test]
    fn check_qmode_period_mirrors_cross_column_check() {
        // single ⇒ quarterly ok; annual/h1 rejected.
        assert!(check_qmode_period(QMode::Single, PeriodType::Q1).is_ok());
        assert!(check_qmode_period(QMode::Single, PeriodType::Q4).is_ok());
        assert!(check_qmode_period(QMode::Single, PeriodType::Annual).is_err());
        assert!(check_qmode_period(QMode::Single, PeriodType::H1).is_err());
        // non-single ⇒ annual/h1/q1/q3 ok; q2/q4 rejected.
        assert!(check_qmode_period(QMode::Cumulative, PeriodType::Annual).is_ok());
        assert!(check_qmode_period(QMode::Point, PeriodType::Q3).is_ok());
        assert!(check_qmode_period(QMode::Na, PeriodType::H1).is_ok());
        assert!(check_qmode_period(QMode::Cumulative, PeriodType::Q2).is_err());
        assert!(check_qmode_period(QMode::Point, PeriodType::Q4).is_err());
        // Message names the offending period_type.
        let msg = check_qmode_period(QMode::Single, PeriodType::Annual).unwrap_err();
        assert!(msg.contains("annual"), "{msg}");
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

    fn fin_row(stmt: Statement, pt: PeriodType, scope: DomainScope, value: f64) -> FinancialRow {
        use crate::domain::financial_row::{AuditStatus, SourceTag, Unit};
        use time::{Date, Month};
        FinancialRow {
            symbol: Symbol::parse("600519").unwrap(),
            name: "贵州茅台".into(),
            period: Date::from_calendar_date(2024, Month::September, 30).unwrap(),
            period_type: pt,
            statement: stmt,
            scope,
            item: "TOTAL_OPERATE_INCOME".into(),
            value,
            unit: Unit::Raw,
            currency: "CNY".into(),
            publish_date: None,
            audit: AuditStatus::Unknown,
            source: SourceTag::EastMoney,
        }
    }

    #[test]
    fn fact_row_from_maps_qmode_and_scope() {
        // income cumulative
        let f = fact_row_from(&fin_row(Statement::Income, PeriodType::Q3, DomainScope::Consolidated, 42.0), false);
        assert_eq!(f.symbol, "600519.CN-A");
        assert_eq!(f.fiscal_year, 2024);
        assert_eq!(f.qmode, QMode::Cumulative);
        assert_eq!(f.scope, Scope::Consolidated);
        assert_eq!(f.raw_key, "TOTAL_OPERATE_INCOME");
        assert_eq!(f.source, "eastmoney");
        assert_eq!(f.value, 42.0);
        assert_eq!(f.currency.as_deref(), Some("CNY"));
        // income single
        let s = fact_row_from(&fin_row(Statement::Income, PeriodType::Q3, DomainScope::Consolidated, 1.0), true);
        assert_eq!(s.qmode, QMode::Single);
        // balance → point, parent scope
        let b = fact_row_from(&fin_row(Statement::Balance, PeriodType::Q3, DomainScope::Parent, 1.0), false);
        assert_eq!(b.qmode, QMode::Point);
        assert_eq!(b.scope, Scope::Parent);
        // indicator → na
        let i = fact_row_from(&fin_row(Statement::Indicator, PeriodType::Q3, DomainScope::Consolidated, 1.0), false);
        assert_eq!(i.qmode, QMode::Na);
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
