//! Service layer for `sift metric` (controlled vocabulary) and
//! `sift map` (agent-maintained `raw_key → std_key` mappings).
//!
//! Parsing / validation / strict preflight live here; the pure CRUD
//! is in [`crate::store`]. Commands only call these functions.

use std::collections::HashSet;

use crate::app::AppContext;
use crate::error::SiftError;
use crate::service::store;
use crate::service::tsv::{self, col, FromTsvRow};
use crate::store::{MapRow, MetricRow};
// Command-facing result type surfaced through the service layer.
pub use crate::store::BatchOutcome;

/// Allowed `unit_kind` values (mirrors the DuckDB CHECK).
const UNIT_KINDS: [&str; 6] = ["amount", "ratio", "per_share", "shares", "count", "other"];

// ---- metric --------------------------------------------------------------

/// Register/overwrite one metric definition.
pub fn metric_add_one(
    app: &AppContext,
    std_key: &str,
    label: Option<&str>,
    unit_kind: &str,
) -> Result<BatchOutcome, SiftError> {
    let row = MetricRow {
        std_key: non_empty(std_key, "std_key")?.to_string(),
        label: label.map(|s| s.to_string()),
        unit_kind: validated_unit_kind(unit_kind)?.to_string(),
    };
    store(app)?.upsert_metrics(&[row], true)
}

/// Ingest a `#std_key\tlabel\tunit_kind` TSV batch.
pub fn ingest_metrics_tsv(
    app: &AppContext,
    input: &str,
    atomic: bool,
) -> Result<BatchOutcome, SiftError> {
    let (rows, parse_errs) = tsv::parse_tsv::<MetricRow>(input);
    if atomic {
        if let Some((line, why)) = parse_errs.first() {
            return Err(SiftError::Parse(format!("line {line}: {why}")));
        }
    }
    let mut out = store(app)?.upsert_metrics(&rows, atomic)?;
    if !atomic && !parse_errs.is_empty() {
        out.skipped.extend(parse_errs);
        out.skipped.sort_by_key(|(i, _)| *i);
    }
    Ok(out)
}

pub fn list_metrics(app: &AppContext) -> Result<(Vec<String>, Vec<Vec<String>>), SiftError> {
    store(app)?.list_metrics()
}

pub fn metric_remove(app: &AppContext, std_key: &str, cascade: bool) -> Result<usize, SiftError> {
    store(app)?.delete_metric(non_empty(std_key, "std_key")?, cascade)
}

// ---- map ------------------------------------------------------------------

/// Set one mapping. Strict: the `std_key` must already be registered
/// (friendlier than waiting for the DuckDB FK error).
pub fn map_set_one(
    app: &AppContext,
    source: &str,
    raw_key: &str,
    std_key: &str,
) -> Result<BatchOutcome, SiftError> {
    let row = MapRow {
        source: non_empty(source, "source")?.to_string(),
        raw_key: non_empty(raw_key, "raw_key")?.to_string(),
        std_key: non_empty(std_key, "std_key")?.to_string(),
    };
    let known = known_std_keys(app)?;
    if !known.contains(&row.std_key) {
        return Err(unknown_std_key(&row.std_key));
    }
    store(app)?.upsert_map(&[row], true)
}

/// Ingest a `#source\traw_key\tstd_key` TSV batch, strictly validating
/// each `std_key` against the registered metrics.
pub fn ingest_map_tsv(
    app: &AppContext,
    input: &str,
    atomic: bool,
) -> Result<BatchOutcome, SiftError> {
    let (rows, parse_errs) = tsv::parse_tsv::<MapRow>(input);
    let known = known_std_keys(app)?;
    // Preflight indices are 1-based over the successfully-parsed rows.
    let preflight: Vec<(usize, String)> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| !known.contains(&r.std_key))
        .map(|(i, r)| (i + 1, unknown_std_key(&r.std_key).to_string()))
        .collect();

    if atomic {
        if let Some((line, why)) = parse_errs.first() {
            return Err(SiftError::Parse(format!("line {line}: {why}")));
        }
        if let Some((line, why)) = preflight.first() {
            return Err(SiftError::Parse(format!("line {line}: {why}")));
        }
        return store(app)?.upsert_map(&rows, true);
    }

    // Skip-invalid: drop preflight failures, write the rest, and merge
    // both failure kinds into the outcome.
    let bad: HashSet<usize> = preflight.iter().map(|(i, _)| *i).collect();
    let good: Vec<MapRow> = rows
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !bad.contains(&(i + 1)))
        .map(|(_, r)| r)
        .collect();
    let mut out = store(app)?.upsert_map(&good, false)?;
    out.skipped.extend(parse_errs);
    out.skipped.extend(preflight);
    out.skipped.sort_by_key(|(i, _)| *i);
    Ok(out)
}

pub fn list_map(
    app: &AppContext,
    source: Option<&str>,
) -> Result<(Vec<String>, Vec<Vec<String>>), SiftError> {
    store(app)?.list_map(source)
}

pub fn map_remove(app: &AppContext, source: &str, raw_key: &str) -> Result<usize, SiftError> {
    store(app)?.delete_map(non_empty(source, "source")?, non_empty(raw_key, "raw_key")?)
}

// ---- TSV row shapes -------------------------------------------------------

impl FromTsvRow for MetricRow {
    fn from_fields(header: &[String], fields: &[&str]) -> Result<Self, String> {
        let std_key = col(header, fields, "std_key")
            .filter(|s| !s.is_empty())
            .ok_or("missing std_key")?
            .to_string();
        let label = col(header, fields, "label")
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let unit_kind = col(header, fields, "unit_kind").unwrap_or("amount");
        validated_unit_kind(unit_kind).map_err(|e| e.to_string())?;
        Ok(MetricRow {
            std_key,
            label,
            unit_kind: unit_kind.to_string(),
        })
    }
}

impl FromTsvRow for MapRow {
    fn from_fields(header: &[String], fields: &[&str]) -> Result<Self, String> {
        let get = |name: &str| {
            col(header, fields, name)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("missing {name}"))
        };
        Ok(MapRow {
            source: get("source")?.to_string(),
            raw_key: get("raw_key")?.to_string(),
            std_key: get("std_key")?.to_string(),
        })
    }
}

// ---- helpers --------------------------------------------------------------

fn known_std_keys(app: &AppContext) -> Result<HashSet<String>, SiftError> {
    Ok(store(app)?.metric_keys()?.into_iter().collect())
}

fn unknown_std_key(std_key: &str) -> SiftError {
    SiftError::Parse(format!(
        "unknown std_key {std_key:?}; run `sift metric add {std_key}` first"
    ))
}

fn validated_unit_kind(s: &str) -> Result<&str, SiftError> {
    if UNIT_KINDS.contains(&s) {
        Ok(s)
    } else {
        Err(SiftError::Parse(format!(
            "bad unit_kind {s:?} (one of {})",
            UNIT_KINDS.join("/")
        )))
    }
}

fn non_empty<'a>(s: &'a str, what: &str) -> Result<&'a str, SiftError> {
    let t = s.trim();
    if t.is_empty() {
        return Err(SiftError::Parse(format!("empty {what}")));
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_from_fields_defaults_unit_kind_to_amount() {
        let header: Vec<String> = ["std_key", "label"].iter().map(|s| s.to_string()).collect();
        let r = MetricRow::from_fields(&header, &["revenue", "营业总收入"]).unwrap();
        assert_eq!(r.std_key, "revenue");
        assert_eq!(r.label.as_deref(), Some("营业总收入"));
        assert_eq!(r.unit_kind, "amount");
    }

    #[test]
    fn metric_from_fields_rejects_bad_unit_kind() {
        let header: Vec<String> = ["std_key", "unit_kind"].iter().map(|s| s.to_string()).collect();
        let err = MetricRow::from_fields(&header, &["roe", "percentage"]).unwrap_err();
        assert!(err.contains("unit_kind"), "{err}");
    }

    #[test]
    fn map_from_fields_requires_all_three() {
        let header: Vec<String> = ["source", "raw_key", "std_key"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(MapRow::from_fields(&header, &["eastmoney", "", "revenue"]).is_err());
        let ok = MapRow::from_fields(&header, &["eastmoney", "TOTAL_OPERATE_INCOME", "revenue"]).unwrap();
        assert_eq!(ok.raw_key, "TOTAL_OPERATE_INCOME");
    }
}
