//! Long-format financial row + the enums it composes.
//!
//! One `FinancialRow` = one `(symbol, period, statement, scope, item)`
//! cell, post-normalization. The renderer pivots multiple rows back to
//! wide table form at output time; storage / merging is always long.

use time::Date;

use crate::domain::period::PeriodType;
use crate::domain::Symbol;

/// One normalized fact: a single line item for one symbol, one report
/// period. `item` is the standardized Chinese name if the dictionary
/// resolves it; otherwise the upstream raw label (in which case the
/// raw label is also reported through the unmapped collector — see
/// [`crate::domain::items_dict::record_unmapped`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FinancialRow {
    pub symbol: Symbol,
    /// Security short name as published by the upstream.
    pub name: String,
    /// Report-end date (e.g. 2024-12-31 for an annual).
    pub period: Date,
    pub period_type: PeriodType,
    pub statement: Statement,
    pub scope: Scope,
    /// Standardized Chinese item name, or upstream raw if unmapped.
    pub item: String,
    /// Value already scaled by `unit` (i.e. divided by `unit.factor()`).
    pub value: f64,
    pub unit: Unit,
    /// Upstream-declared currency (`"CNY"` / `"人民币"` / `"港元"` / ...).
    pub currency: String,
    pub publish_date: Option<Date>,
    pub audit: AuditStatus,
    pub source: SourceTag,
}

/// Which of the four canonical statements this row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Statement {
    Income,
    Balance,
    Cashflow,
    Indicator,
}

impl Statement {
    /// All four variants — handy for command-layer iteration and tests.
    pub const ALL: &'static [Statement] = &[
        Statement::Income,
        Statement::Balance,
        Statement::Cashflow,
        Statement::Indicator,
    ];

    /// Lower-case string used in CLI args and JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Income => "income",
            Self::Balance => "balance",
            Self::Cashflow => "cashflow",
            Self::Indicator => "indicator",
        }
    }
}

/// Consolidated (合并) vs Parent-only (母公司) statement. Parent is
/// supported on A-share only; HK / US upstreams do not publish
/// parent-only statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    Consolidated,
    Parent,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Consolidated => "consolidated",
            Self::Parent => "parent",
        }
    }
}

/// Numeric magnitude unit applied to `value`. `Raw` is always the
/// default; `--unit wan` / `--unit yi` is a *display* convenience and
/// does not change which upstream is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Raw upstream value, no scaling.
    Raw,
    /// 万 — divide by `1e4`.
    Wan,
    /// 亿 — divide by `1e8`.
    Yi,
}

impl Unit {
    /// Scaling factor that `raw_value / factor()` is reported as.
    pub fn factor(self) -> f64 {
        match self {
            Self::Raw => 1.0,
            Self::Wan => 1e4,
            Self::Yi => 1e8,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Wan => "wan",
            Self::Yi => "yi",
        }
    }
}

/// Whether the report has been externally audited. Most A-share annuals
/// are audited; quarterly reports are typically `Unaudited`. Upstream
/// may not disclose audit status — represented by `Unknown` rather than
/// guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditStatus {
    Audited,
    Unaudited,
    Unknown,
}

impl AuditStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Audited => "audited",
            Self::Unaudited => "unaudited",
            Self::Unknown => "unknown",
        }
    }
}

/// Identifies the data source that produced this row. Each
/// [`crate::sources`] adapter pins a single tag; downstream renders it
/// in the `source` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceTag {
    EastMoney,
    Sina,
}

impl SourceTag {
    /// Every registered source. Used for CLI enumeration and as the
    /// reverse-lookup table for [`SourceTag::from_name`].
    pub const ALL: &'static [SourceTag] = &[SourceTag::EastMoney, SourceTag::Sina];

    /// Lower-case label that flows through `_source`, the cache, and
    /// `--source` CLI parsing. Stable string contract — changing this
    /// would invalidate the on-disk cache rows.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EastMoney => "eastmoney",
            Self::Sina => "sina",
        }
    }

    /// Inverse of [`as_str`]. Unknown names return `None` — callers
    /// that need a default (e.g. legacy cache rows written before a
    /// new source was added) fall back to `SourceTag::EastMoney`.
    pub fn from_name(name: &str) -> Option<SourceTag> {
        Self::ALL.iter().copied().find(|t| t.as_str() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_all_lists_four_in_order() {
        assert_eq!(
            Statement::ALL,
            &[
                Statement::Income,
                Statement::Balance,
                Statement::Cashflow,
                Statement::Indicator,
            ]
        );
    }

    #[test]
    fn statement_as_str_table() {
        assert_eq!(Statement::Income.as_str(), "income");
        assert_eq!(Statement::Balance.as_str(), "balance");
        assert_eq!(Statement::Cashflow.as_str(), "cashflow");
        assert_eq!(Statement::Indicator.as_str(), "indicator");
    }

    #[test]
    fn scope_as_str_table() {
        assert_eq!(Scope::Consolidated.as_str(), "consolidated");
        assert_eq!(Scope::Parent.as_str(), "parent");
    }

    #[test]
    fn unit_factor_and_label_table() {
        assert_eq!(Unit::Raw.factor(), 1.0);
        assert_eq!(Unit::Wan.factor(), 1e4);
        assert_eq!(Unit::Yi.factor(), 1e8);
        assert_eq!(Unit::Raw.as_str(), "raw");
        assert_eq!(Unit::Wan.as_str(), "wan");
        assert_eq!(Unit::Yi.as_str(), "yi");
    }

    #[test]
    fn audit_status_as_str_table() {
        assert_eq!(AuditStatus::Audited.as_str(), "audited");
        assert_eq!(AuditStatus::Unaudited.as_str(), "unaudited");
        assert_eq!(AuditStatus::Unknown.as_str(), "unknown");
    }

    #[test]
    fn source_tag_as_str_table() {
        assert_eq!(SourceTag::EastMoney.as_str(), "eastmoney");
        assert_eq!(SourceTag::Sina.as_str(), "sina");
    }

    #[test]
    fn source_tag_from_name_round_trips_through_as_str() {
        for &t in SourceTag::ALL {
            assert_eq!(SourceTag::from_name(t.as_str()), Some(t));
        }
        assert_eq!(SourceTag::from_name("unknown"), None);
        assert_eq!(SourceTag::from_name(""), None);
    }
}
