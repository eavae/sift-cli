//! DuckDB-backed financial-row cache.
//!
//! Schema is the single table `financials`, keyed by
//! `blake3(symbol | statement | period_end | scope | source)`. Each
//! row stores the entire `Vec<FinancialRow>` for that `(symbol,
//! period, statement, scope, source)` tuple as a JSON BLOB — that
//! way new items can be added to the dictionary or schema without
//! ALTER TABLE pain.
//!
//! Reads consult [`crate::cache::ttl`] to skip stale rows; writes are
//! always upserts (never deleted).
//!
//! ## DuckDB access discipline
//! Every SQL call routes through [`crate::cache::with_duckdb`]: the
//! struct stores only the path, never a long-lived `Connection`. This
//! is what lets two concurrent `sift financials` processes share the
//! same file — see the module docs of [`crate::cache`] for the full
//! invariant.

mod stored;

use std::path::{Path, PathBuf};

use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

use self::stored::StoredRow;
use crate::cache::{ttl, with_duckdb, DuckAccess};
use crate::domain::market::Market;
use crate::domain::{FinancialRow, Period, PeriodType, Scope, SourceTag, Statement, Symbol};
use crate::error::SiftError;

/// On-disk financial cache. Lightweight — only the path; every SQL
/// call opens a fresh DuckDB connection via
/// [`crate::cache::with_duckdb`] and drops it immediately. `Send +
/// Sync` without interior locking; cross-process contention is
/// handled by the retry ladder inside `with_duckdb`.
pub struct FinancialCache {
    db_path: PathBuf,
}

impl FinancialCache {
    /// Open (or create) the cache at `path`. Idempotent — the schema
    /// is set up via `CREATE TABLE IF NOT EXISTS` and the connection
    /// is released as soon as `with_duckdb` returns.
    pub fn open(path: &Path) -> Result<Self, SiftError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SiftError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }
        with_duckdb(path, DuckAccess::Write, |conn| {
            conn.execute_batch(SCHEMA_SQL)
                .map_err(|e| SiftError::Io(format!("duckdb schema: {e}")))
        })?;
        Ok(Self {
            db_path: path.to_path_buf(),
        })
    }

    /// Look up cached rows for one `(symbol, statement, period, scope,
    /// source)` tuple. Returns `None` for both "no row" and "row is
    /// stale per [`crate::cache::ttl::TtlBucket`]".
    pub fn get(
        &self,
        sym: &Symbol,
        stmt: Statement,
        period: Period,
        scope: Scope,
        source: SourceTag,
        today: Date,
    ) -> Option<Vec<FinancialRow>> {
        let key = cache_key(sym, stmt, period, scope, source);
        let bucket = ttl::bucket_for(period.end_date(), today);

        let blob_and_ts = with_duckdb(&self.db_path, DuckAccess::Read, |conn| {
            let mut stmt_q = conn
                .prepare("SELECT rows_json, fetched_at FROM financials WHERE key = ?")
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            let mut iter = stmt_q
                .query_map([&key as &dyn duckdb::ToSql], |row| {
                    let blob: Vec<u8> = row.get(0)?;
                    let fetched_at_s: String = row.get(1)?;
                    Ok((blob, fetched_at_s))
                })
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            Ok(iter.next().and_then(Result::ok))
        })
        .ok()
        .flatten()?;
        let (blob, fetched_at_s) = blob_and_ts;
        let fetched_at = OffsetDateTime::parse(&fetched_at_s, &Rfc3339).ok()?;
        if !ttl::is_fresh(fetched_at, bucket) {
            return None;
        }
        let stored: Vec<StoredRow> = serde_json::from_slice(&blob).ok()?;
        // Re-normalize `item` against the current dictionary. Rows
        // written before a dictionary extension may carry the raw
        // upstream English label; this lookup is O(1) and lets dict
        // updates apply to already-cached rows without waiting for
        // TTL expiry. Items still unknown to the dict pass through
        // unchanged (and get re-recorded into the unmapped collector).
        let dict = crate::domain::items_dict::dict();
        Some(
            stored
                .into_iter()
                .map(|sr| {
                    let mut row = sr.into_row();
                    row.item = dict.normalize(&row.item);
                    row
                })
                .collect(),
        )
    }

    /// Upsert all rows from one source. Rows are grouped by
    /// `(symbol, period, statement, scope)` and written one DB row
    /// per group as a JSON BLOB. Always writes — even for empty
    /// groups (so a "we asked and got nothing" answer can also be
    /// cached, though that path is not currently exercised).
    pub fn put(&self, rows: &[FinancialRow], source: SourceTag) -> Result<(), SiftError> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();

        // Group rows by (symbol, period, statement, scope) outside
        // the DB call so the connection holds the lock for the SQL
        // pass only, not the grouping pass.
        use std::collections::BTreeMap;
        let mut groups: BTreeMap<GroupKey, Vec<&FinancialRow>> = BTreeMap::new();
        for r in rows {
            let key = GroupKey {
                symbol_code: r.symbol.code.clone(),
                market: r.symbol.market as u8,
                period: r
                    .period
                    .format(&time::format_description::well_known::Iso8601::DATE)
                    .unwrap_or_default(),
                statement: r.statement,
                scope: r.scope,
            };
            groups.entry(key).or_default().push(r);
        }

        with_duckdb(&self.db_path, DuckAccess::Write, |conn| {
            // `unchecked_transaction` takes `&Connection` so it
            // composes with `with_duckdb`'s `&Connection` closure
            // argument; `Connection::transaction` would need `&mut`
            // and isn't reachable from here.
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| SiftError::Io(format!("duckdb tx: {e}")))?;
            for (gkey, group) in &groups {
                let sym = Symbol {
                    code: gkey.symbol_code.clone(),
                    market: market_from_u8(gkey.market),
                };
                let period = parse_period_from_iso(&gkey.period)?;
                let key = cache_key(&sym, gkey.statement, period, gkey.scope, source);
                let period_end = period.end_date();
                let period_type = period.period_type().unwrap_or(PeriodType::Annual);
                let publish_date = group
                    .iter()
                    .find_map(|r| r.publish_date)
                    .map(|d| {
                        d.format(&time::format_description::well_known::Iso8601::DATE)
                            .unwrap_or_default()
                    });

                let stored: Vec<StoredRow> =
                    group.iter().map(|r| StoredRow::from_row(r)).collect();
                let json = serde_json::to_vec(&stored)
                    .map_err(|e| SiftError::Internal(format!("json: {e}")))?;

                tx.execute(
                    INSERT_SQL,
                    duckdb::params![
                        &key,
                        &sym.code,
                        gkey.statement.as_str(),
                        period_end
                            .format(&time::format_description::well_known::Iso8601::DATE)
                            .unwrap_or_default(),
                        period_type.as_str(),
                        gkey.scope.as_str(),
                        source.as_str(),
                        &json,
                        &now,
                        publish_date,
                    ],
                )
                .map_err(|e| SiftError::Io(format!("duckdb insert: {e}")))?;
            }
            tx.commit()
                .map_err(|e| SiftError::Io(format!("duckdb commit: {e}")))?;
            Ok(())
        })
    }

    /// Test helper: overwrite the `fetched_at` of any row with the
    /// given key. Lets cache tests simulate stale rows without
    /// waiting 24h.
    #[cfg(test)]
    pub(crate) fn force_fetched_at(
        &self,
        sym: &Symbol,
        stmt: Statement,
        period: Period,
        scope: Scope,
        source: SourceTag,
        when: OffsetDateTime,
    ) -> Result<(), SiftError> {
        let key = cache_key(sym, stmt, period, scope, source);
        let when_s = when.format(&Rfc3339).unwrap_or_default();
        with_duckdb(&self.db_path, DuckAccess::Write, |conn| {
            conn.execute(
                "UPDATE financials SET fetched_at = ? WHERE key = ?",
                duckdb::params![&when_s, &key],
            )
            .map_err(|e| SiftError::Io(format!("duckdb update: {e}")))?;
            Ok(())
        })
    }

    /// Test helper: total row count in the table.
    #[cfg(test)]
    pub(crate) fn row_count(&self) -> usize {
        with_duckdb(&self.db_path, DuckAccess::Read, |conn| {
            let mut s = conn
                .prepare("SELECT COUNT(*) FROM financials")
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            let mut iter = s
                .query_map([], |r| r.get::<usize, i64>(0))
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            let n = iter
                .next()
                .and_then(Result::ok)
                .unwrap_or(0) as usize;
            Ok(n)
        })
        .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Schema + SQL
// ---------------------------------------------------------------------------

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS financials (
    key          BLOB PRIMARY KEY,
    symbol       TEXT NOT NULL,
    statement    TEXT NOT NULL,
    period_end   TEXT NOT NULL,
    period_type  TEXT NOT NULL,
    scope        TEXT NOT NULL,
    source       TEXT NOT NULL,
    rows_json    BLOB NOT NULL,
    fetched_at   TEXT NOT NULL,
    publish_date TEXT
);
CREATE INDEX IF NOT EXISTS idx_symbol_period ON financials(symbol, period_end);
"#;

const INSERT_SQL: &str = r#"
INSERT OR REPLACE INTO financials
    (key, symbol, statement, period_end, period_type, scope, source,
     rows_json, fetched_at, publish_date)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#;

// ---------------------------------------------------------------------------
// Key + grouping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GroupKey {
    symbol_code: String,
    market: u8,
    period: String,
    statement: Statement,
    scope: Scope,
}

fn cache_key(
    sym: &Symbol,
    stmt: Statement,
    period: Period,
    scope: Scope,
    source: SourceTag,
) -> Vec<u8> {
    let mut h = blake3::Hasher::new();
    h.update(sym.code.as_bytes());
    h.update(b"|");
    h.update(sym.market.as_lower().as_bytes());
    h.update(b"|");
    h.update(stmt.as_str().as_bytes());
    h.update(b"|");
    h.update(
        period
            .end_date()
            .format(&time::format_description::well_known::Iso8601::DATE)
            .unwrap_or_default()
            .as_bytes(),
    );
    h.update(b"|");
    h.update(scope.as_str().as_bytes());
    h.update(b"|");
    h.update(source.as_str().as_bytes());
    h.finalize().as_bytes().to_vec()
}

fn parse_period_from_iso(s: &str) -> Result<Period, SiftError> {
    // The grouping always uses the period's end date (YYYY-MM-DD), and
    // `Period::parse` auto-normalizes aligned ends to the variant.
    let head = s.split('T').next().unwrap_or(s);
    Period::parse(head)
}

pub(super) fn market_from_u8(b: u8) -> Market {
    match b {
        0 => Market::CnA,
        1 => Market::Hk,
        2 => Market::Us,
        _ => Market::CnA,
    }
}


// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AuditStatus, Unit};
    use tempfile::TempDir;
    use time::{Duration, Month};

    fn make_cache() -> (TempDir, FinancialCache) {
        let tmp = TempDir::new().unwrap();
        let cache = FinancialCache::open(&tmp.path().join("financials.duckdb")).unwrap();
        (tmp, cache)
    }

    fn sample_row(period_end: Date, item: &str, value: f64, source: SourceTag) -> FinancialRow {
        FinancialRow {
            symbol: Symbol {
                code: "600519".into(),
                market: Market::CnA,
            },
            name: "贵州茅台".into(),
            period: period_end,
            period_type: PeriodType::from_date(period_end).unwrap_or(PeriodType::Annual),
            statement: Statement::Income,
            scope: Scope::Consolidated,
            item: item.into(),
            value,
            unit: Unit::Raw,
            currency: "CNY".into(),
            publish_date: None,
            audit: AuditStatus::Unknown,
            source,
        }
    }

    fn date(y: i32, m: u8, d: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), d).unwrap()
    }

    #[test]
    fn open_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("financials.duckdb");
        let _ = FinancialCache::open(&path).unwrap();
        let _ = FinancialCache::open(&path).unwrap();
    }

    #[test]
    fn put_get_round_trip_returns_identical_rows() {
        let (_tmp, cache) = make_cache();
        let period = date(2025, 12, 31);
        let rows = vec![
            sample_row(period, "营业总收入", 172054171891.0, SourceTag::EastMoney),
            sample_row(period, "归母净利润", 82320067102.0, SourceTag::EastMoney),
        ];
        cache.put(&rows, SourceTag::EastMoney).unwrap();
        let got = cache
            .get(
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
    fn stale_recent_bucket_returns_none() {
        let (_tmp, cache) = make_cache();
        let period = date(2026, 3, 31);
        let rows = vec![sample_row(period, "营业总收入", 1.0, SourceTag::EastMoney)];
        cache.put(&rows, SourceTag::EastMoney).unwrap();
        cache
            .force_fetched_at(
                &rows[0].symbol,
                Statement::Income,
                Period::Q1(2026),
                Scope::Consolidated,
                SourceTag::EastMoney,
                OffsetDateTime::now_utc() - Duration::hours(25),
            )
            .unwrap();
        let got = cache.get(
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
    fn old_bucket_is_always_fresh_even_with_ancient_fetched_at() {
        let (_tmp, cache) = make_cache();
        let period = date(2020, 12, 31);
        let rows = vec![sample_row(period, "营业总收入", 1.0, SourceTag::EastMoney)];
        cache.put(&rows, SourceTag::EastMoney).unwrap();
        cache
            .force_fetched_at(
                &rows[0].symbol,
                Statement::Income,
                Period::Annual(2020),
                Scope::Consolidated,
                SourceTag::EastMoney,
                OffsetDateTime::now_utc() - Duration::days(365 * 5),
            )
            .unwrap();
        let got = cache.get(
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
    fn upsert_replaces_existing_row_without_growing_count() {
        let (_tmp, cache) = make_cache();
        let period = date(2025, 12, 31);
        let rows_v1 = vec![sample_row(period, "营业总收入", 1.0, SourceTag::EastMoney)];
        let rows_v2 = vec![sample_row(period, "营业总收入", 2.0, SourceTag::EastMoney)];
        cache.put(&rows_v1, SourceTag::EastMoney).unwrap();
        assert_eq!(cache.row_count(), 1);
        cache.put(&rows_v2, SourceTag::EastMoney).unwrap();
        assert_eq!(cache.row_count(), 1, "upsert, not insert");
        let got = cache
            .get(
                &rows_v1[0].symbol,
                Statement::Income,
                Period::Annual(2025),
                Scope::Consolidated,
                SourceTag::EastMoney,
                date(2026, 5, 20),
            )
            .unwrap();
        assert_eq!(got[0].value, 2.0);
    }

    #[test]
    fn different_sources_do_not_share_cache_rows() {
        let (_tmp, cache) = make_cache();
        let period = date(2025, 12, 31);
        let em_rows = vec![sample_row(period, "营业总收入", 1.0, SourceTag::EastMoney)];
        let sina_rows = vec![sample_row(period, "营业总收入", 2.0, SourceTag::Sina)];
        cache.put(&em_rows, SourceTag::EastMoney).unwrap();
        cache.put(&sina_rows, SourceTag::Sina).unwrap();
        assert_eq!(cache.row_count(), 2);

        let em_got = cache
            .get(
                &em_rows[0].symbol,
                Statement::Income,
                Period::Annual(2025),
                Scope::Consolidated,
                SourceTag::EastMoney,
                date(2026, 5, 20),
            )
            .unwrap();
        let sina_got = cache
            .get(
                &sina_rows[0].symbol,
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
}
