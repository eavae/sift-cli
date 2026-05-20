//! Domain types shared across data sources and commands.
//!
//! - `Symbol` parses user-supplied stock identifiers (F1).
//! - `Market` / `Board` plus `infer_board` / `em_secid_prefix` (F1).
//! - `Period` / `PeriodType` map user-facing period literals to report
//!   end-dates (F2).
//! - `FinancialRow` plus `Statement` / `Scope` / `Unit` / `AuditStatus`
//!   / `SourceTag` are the long-format domain types financials sources
//!   produce (F2).
//! - `Query` is the input to the financial-source dispatch layer (F2).
//! - `items_dict` ships the upstream-label → standardized-Chinese
//!   dictionary plus the unmapped-label collector (F2).

#![allow(dead_code)]

pub mod announcement;
pub mod financial_query;
pub mod financial_row;
pub mod items_dict;
pub mod market;
pub mod period;
pub mod symbol;

#[allow(unused_imports)]
pub use announcement::{categories, lookup as lookup_announcement_type, AnnouncementRow, Category};

#[allow(unused_imports)] // consumed by F1 stories and Story 02+; re-exported for ergonomics.
pub use market::{em_secid_prefix, infer_board, Board, Market};
#[allow(unused_imports)]
pub use symbol::Symbol;

#[allow(unused_imports)]
pub use financial_query::Query;
#[allow(unused_imports)]
pub use financial_row::{AuditStatus, FinancialRow, Scope, SourceTag, Statement, Unit};
#[allow(unused_imports)]
pub use items_dict::{default_items, dict, drain_unmapped, record_unmapped, ItemEntry, ItemsDict};
#[allow(unused_imports)]
pub use period::{Period, PeriodType};
