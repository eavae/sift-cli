//! JSON-friendly serialization of [`FinancialRow`] for the DuckDB
//! `rows_json` BLOB column.
//!
//! We do not derive `Serialize` on [`FinancialRow`] itself because the
//! production `--format json` output uses a different shape; this is
//! purely the cache's storage format. Keeping it isolated also means
//! schema-of-disk changes (e.g. renaming a field, widening a value)
//! don't ripple into the public JSON contract.

use serde::{Deserialize, Serialize};
use time::Date;

use super::market_from_u8;
use crate::domain::{
    AuditStatus, FinancialRow, PeriodType, Scope, SourceTag, Statement, Symbol, Unit,
};

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct StoredRow {
    symbol_code: String,
    symbol_market: u8,
    name: String,
    period: String, // YYYY-MM-DD
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

impl StoredRow {
    pub(super) fn from_row(r: &FinancialRow) -> Self {
        Self {
            symbol_code: r.symbol.code.clone(),
            symbol_market: r.symbol.market as u8,
            name: r.name.clone(),
            period: r
                .period
                .format(&time::format_description::well_known::Iso8601::DATE)
                .unwrap_or_default(),
            period_type: r.period_type.as_str().into(),
            statement: r.statement.as_str().into(),
            scope: r.scope.as_str().into(),
            item: r.item.clone(),
            value: r.value,
            unit: r.unit.as_str().into(),
            currency: r.currency.clone(),
            publish_date: r.publish_date.map(|d| {
                d.format(&time::format_description::well_known::Iso8601::DATE)
                    .unwrap_or_default()
            }),
            audit: r.audit.as_str().into(),
            source: r.source.as_str().into(),
        }
    }

    pub(super) fn into_row(self) -> FinancialRow {
        FinancialRow {
            symbol: Symbol {
                code: self.symbol_code,
                market: market_from_u8(self.symbol_market),
            },
            name: self.name,
            period: parse_date(&self.period).unwrap_or_else(epoch_date),
            period_type: self.period_type.parse().unwrap_or(PeriodType::Annual),
            statement: self.statement.parse().unwrap_or(Statement::Income),
            scope: self.scope.parse().unwrap_or(Scope::Consolidated),
            item: self.item,
            value: self.value,
            unit: self.unit.parse().unwrap_or(Unit::Raw),
            currency: self.currency,
            publish_date: self.publish_date.as_deref().and_then(parse_date),
            audit: self.audit.parse().unwrap_or(AuditStatus::Unknown),
            source: SourceTag::from_name(&self.source).unwrap_or(SourceTag::EastMoney),
        }
    }
}

fn parse_date(s: &str) -> Option<Date> {
    let mut parts = s.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    let d: u8 = parts.next()?.parse().ok()?;
    Date::from_calendar_date(y, time::Month::try_from(m).ok()?, d).ok()
}

fn epoch_date() -> Date {
    Date::from_calendar_date(1970, time::Month::January, 1).unwrap()
}
