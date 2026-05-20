//! Query input for the financial-source dispatch layer (Story 02).
//!
//! Plain data — no methods. Source adapters consume `&Query` in
//! `FinancialSource::fetch` and return `Vec<FinancialRow>`.

use crate::domain::financial_row::{Scope, Statement};
use crate::domain::period::Period;
use crate::domain::Symbol;

/// One financial-data lookup: which symbol, which statement, which
/// periods, and which scope (consolidated vs parent-only).
#[derive(Debug, Clone)]
pub struct Query {
    pub symbol: Symbol,
    pub statement: Statement,
    pub periods: Vec<Period>,
    pub scope: Scope,
}
