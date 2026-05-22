//! Domain types shared across data sources and commands.
//!
//! - `Symbol` parses user-supplied stock identifiers (F1).
//! - `Market` / `Board` plus `infer_board` (F1).
//! - `Period` / `PeriodType` map user-facing period literals to report
//!   end-dates (F2).
//! - `FinancialRow` plus `Statement` / `Scope` / `Unit` / `AuditStatus`
//!   / `SourceTag` are the long-format domain types financials sources
//!   produce (F2).
//! - `Query` is the input to the financial-source dispatch layer (F2).
//! - `items_dict` ships the upstream-label → standardized-Chinese
//!   dictionary plus the unmapped-label collector (F2).

pub mod announcement;
pub mod bars;
pub mod financial_query;
pub mod financial_row;
pub mod items_dict;
pub mod market;
pub mod period;
pub mod quote;
pub mod symbol;

pub use symbol::Symbol;

pub use financial_query::Query;
pub use financial_row::{AuditStatus, FinancialRow, Scope, SourceTag, Statement, Unit};
pub use period::{Period, PeriodType};
