//! Domain types shared across data sources and commands.
//!
//! - `Symbol` parses user-supplied stock identifiers.
//! - `Market` / `Board` plus `infer_board`.
//! - `Period` / `PeriodType` map user-facing period literals to report
//!   end-dates.
//! - `FinancialRow` plus `Statement` / `Scope` / `Unit` / `AuditStatus`
//!   / `SourceTag` are the long-format domain types financials sources
//!   produce.
//! - `Query` is the input to the financial-source dispatch layer.
//!
//! Item labels are passed through **verbatim** from each upstream —
//! there is no name-normalization dictionary. A-share EM statements
//! therefore surface EM's raw English column codes
//! (`TOTAL_OPERATE_INCOME`), while HK / sina surface their native
//! Chinese labels. `--items` filters against those raw labels.

pub mod announcement;
pub mod bars;
pub mod financial_query;
pub mod financial_row;
pub mod market;
pub mod period;
pub mod quote;
pub mod single_quarter;
pub mod symbol;

pub use symbol::Symbol;

pub use financial_query::Query;
pub use financial_row::{AuditStatus, FinancialRow, Scope, SourceTag, Statement, Unit};
pub use period::{Period, PeriodType};
