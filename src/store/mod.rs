//! Storage layer — the persistent, user-curated financial **fact
//! store** (`~/.sift/facts.duckdb`).
//!
//! [`FactStore`] is **pure CRUD**: it bootstraps the star schema, does
//! typed UPSERT/DELETE of already-built rows, and runs read-only /
//! escape-hatch SQL. It contains **no** business logic — no TSV
//! parsing, no `FinancialRow → FactRow` mapping, no best-effort
//! warning policy. Those live one layer up in [`crate::service`].
//!
//! Every DuckDB touch goes through [`crate::db::with_duckdb`] (the
//! neutral access primitive shared with the disposable
//! [`crate::cache::record`]); connections are opened per call and
//! dropped immediately, so parallel `sift` processes cooperate on the
//! one file.
//!
//! ## Schema (star, strongly constrained)
//! `symbols` / `metrics` / `key_map` (dimension + registry) and the
//! long `facts` table, plus the `v_facts` view that computes
//! `period` / `period_end` / `key` at query time so an agent never
//! writes a derivable value. See the f6 README for the rationale.

use std::path::PathBuf;

use time::{Date, OffsetDateTime};

use crate::db::{with_duckdb, DuckAccess};
use crate::domain::period::PeriodType;
use crate::error::SiftError;

/// Single-transaction bootstrap. Idempotent (`IF NOT EXISTS`), run on
/// every [`FactStore::open`]. Kept verbatim in sync with the f6 README.
const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS symbols (
  symbol     TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  market     TEXT NOT NULL CHECK(market IN ('CN-A','HK','US')),
  board      TEXT,
  created_at TIMESTAMP NOT NULL
);
CREATE TABLE IF NOT EXISTS metrics (
  std_key    TEXT PRIMARY KEY,
  label      TEXT,
  unit_kind  TEXT NOT NULL DEFAULT 'amount'
             CHECK(unit_kind IN ('amount','ratio','per_share','shares','count','other')),
  created_at TIMESTAMP NOT NULL
);
CREATE TABLE IF NOT EXISTS key_map (
  source  TEXT NOT NULL, raw_key TEXT NOT NULL,
  std_key TEXT NOT NULL REFERENCES metrics(std_key),
  PRIMARY KEY (source, raw_key)
);
CREATE TABLE IF NOT EXISTS facts (
  symbol       TEXT NOT NULL REFERENCES symbols(symbol),
  fiscal_year  INTEGER NOT NULL CHECK(fiscal_year BETWEEN 1990 AND 2100),
  period_type  TEXT NOT NULL CHECK(period_type IN ('annual','h1','q1','q2','q3','q4')),
  qmode        TEXT NOT NULL CHECK(qmode IN ('cumulative','single','point','na')),
  scope        TEXT NOT NULL CHECK(scope IN ('consolidated','parent','na')),
  raw_key      TEXT NOT NULL,
  source       TEXT NOT NULL,
  value        DOUBLE NOT NULL,
  currency     TEXT,
  publish_date DATE,
  created_at   TIMESTAMP NOT NULL,
  PRIMARY KEY (symbol, fiscal_year, period_type, qmode, scope, raw_key, source),
  CHECK ( (qmode='single'  AND period_type IN ('q1','q2','q3','q4'))
       OR (qmode<>'single' AND period_type IN ('annual','h1','q1','q3')) )
);
CREATE VIEW IF NOT EXISTS v_facts AS
SELECT f.symbol, s.name, s.market, s.board,
       f.fiscal_year, f.period_type, f.qmode, f.scope,
       f.fiscal_year::TEXT || CASE f.period_type WHEN 'annual' THEN 'A'
            ELSE upper(f.period_type) END                        AS period,
       make_date(f.fiscal_year,
         CASE f.period_type WHEN 'q1' THEN 3 WHEN 'q2' THEN 6 WHEN 'h1' THEN 6
                            WHEN 'q3' THEN 9 ELSE 12 END,
         CASE f.period_type WHEN 'q1' THEN 31 WHEN 'q3' THEN 30 WHEN 'q2' THEN 30
                            WHEN 'h1' THEN 30 ELSE 31 END)         AS period_end,
       f.raw_key, COALESCE(m.std_key, f.raw_key)                   AS key,
       (m.std_key IS NOT NULL)                                     AS mapped,
       f.value, f.currency, f.source, f.publish_date, f.created_at
FROM facts f
LEFT JOIN symbols s USING(symbol)
LEFT JOIN key_map m ON m.source=f.source AND m.raw_key=f.raw_key;
";

/// Accumulation mode of a stored value. Drives the cross-column CHECK
/// (`single` ⇒ q1..q4; everything else ⇒ annual/h1/q1/q3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QMode {
    /// Year-to-date cumulative (A-share income / cashflow default).
    Cumulative,
    /// Single-quarter (derived by `report --qmode single`).
    Single,
    /// Point-in-time balance sheet.
    Point,
    /// Not applicable (ratios / manual facts).
    Na,
}

impl QMode {
    pub fn as_str(self) -> &'static str {
        match self {
            QMode::Cumulative => "cumulative",
            QMode::Single => "single",
            QMode::Point => "point",
            QMode::Na => "na",
        }
    }
}

impl std::str::FromStr for QMode {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "cumulative" => Ok(QMode::Cumulative),
            "single" => Ok(QMode::Single),
            "point" => Ok(QMode::Point),
            "na" => Ok(QMode::Na),
            _ => Err(()),
        }
    }
}

/// Consolidation scope. `Na` covers manual / ratio facts with no
/// consolidated-vs-parent distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Consolidated,
    Parent,
    Na,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Consolidated => "consolidated",
            Scope::Parent => "parent",
            Scope::Na => "na",
        }
    }
}

impl std::str::FromStr for Scope {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "consolidated" => Ok(Scope::Consolidated),
            "parent" => Ok(Scope::Parent),
            "na" => Ok(Scope::Na),
            _ => Err(()),
        }
    }
}

/// One fact row — the storage input contract. Built by
/// [`crate::service::facts`]; `FactStore` never constructs one from
/// raw upstream shapes itself. `symbol` is the canonical
/// `{code}.{MARKET}` form (e.g. `600519.CN-A`); its `.MARKET` suffix
/// seeds the `symbols` stub.
#[derive(Debug, Clone, PartialEq)]
pub struct FactRow {
    pub symbol: String,
    pub fiscal_year: i32,
    pub period_type: PeriodType,
    pub qmode: QMode,
    pub scope: Scope,
    pub raw_key: String,
    pub source: String,
    pub value: f64,
    pub currency: Option<String>,
    pub publish_date: Option<Date>,
    /// Display name for the `symbols` stub. `None` ⇒ use `symbol`
    /// itself (a later real fetch back-fills the true name).
    pub name: Option<String>,
}

/// One controlled-vocabulary metric definition. `unit_kind` is
/// whitelist-checked (service side for a friendly message, DuckDB
/// CHECK as backstop).
#[derive(Debug, Clone, PartialEq)]
pub struct MetricRow {
    pub std_key: String,
    pub label: Option<String>,
    pub unit_kind: String,
}

/// One `(source, raw_key) → std_key` mapping row.
#[derive(Debug, Clone, PartialEq)]
pub struct MapRow {
    pub source: String,
    pub raw_key: String,
    pub std_key: String,
}

/// Identifies one fact row for [`FactStore::delete_fact`].
#[derive(Debug, Clone)]
pub struct FactKey {
    pub symbol: String,
    pub fiscal_year: i32,
    pub period_type: PeriodType,
    pub qmode: QMode,
    pub scope: Scope,
    pub raw_key: String,
    pub source: String,
}

/// Result of a batch upsert. `skipped` is non-empty only in
/// `atomic == false` (skip-invalid) mode; each entry is `(1-based
/// input row number, reason)`.
#[derive(Debug, Default, PartialEq)]
pub struct BatchOutcome {
    pub written: usize,
    pub skipped: Vec<(usize, String)>,
}

/// Outcome of the [`FactStore::execute`] escape hatch: a materialized
/// result set (SELECT-like) or an affected-row count (DML/DDL).
#[derive(Debug, PartialEq)]
pub enum SqlOutcome {
    Rows(Vec<String>, Vec<Vec<String>>),
    Affected(usize),
}

/// Handle to the fact store. Lightweight — just the on-disk path;
/// connections open per call via [`with_duckdb`].
pub struct FactStore {
    db_path: PathBuf,
}

fn io<E: std::fmt::Display>(e: E) -> SiftError {
    SiftError::Io(format!("{e}"))
}

impl FactStore {
    /// Open (creating parent dirs) and bootstrap the schema. Idempotent.
    pub fn open(db_path: PathBuf) -> Result<Self, SiftError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SiftError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }
        with_duckdb(&db_path, DuckAccess::Write, |conn| {
            conn.execute_batch(SCHEMA_SQL)
                .map_err(|e| SiftError::Io(format!("fact store schema: {e}")))
        })?;
        Ok(Self { db_path })
    }

    /// Batch UPSERT of facts. See [`Self::run_batch`] for the
    /// atomic / skip-invalid semantics.
    pub fn upsert_facts(&self, rows: &[FactRow], atomic: bool) -> Result<BatchOutcome, SiftError> {
        self.run_batch(rows, atomic, write_fact)
    }

    /// Batch UPSERT of controlled-vocabulary metric definitions.
    pub fn upsert_metrics(
        &self,
        rows: &[MetricRow],
        atomic: bool,
    ) -> Result<BatchOutcome, SiftError> {
        self.run_batch(rows, atomic, write_metric)
    }

    /// Batch UPSERT of `(source, raw_key) → std_key` mappings. The
    /// `std_key` foreign key to `metrics` is enforced by DuckDB;
    /// callers should preflight for a friendlier message.
    pub fn upsert_map(&self, rows: &[MapRow], atomic: bool) -> Result<BatchOutcome, SiftError> {
        self.run_batch(rows, atomic, write_map)
    }

    /// Shared batch driver. One `with_duckdb(Write)`; `created_at` is
    /// stamped internally with `now_utc()` and handed to `write_one`.
    /// `atomic == true` runs the whole batch in one transaction and
    /// rolls back on the first bad row (error carries its 1-based
    /// number). `atomic == false` writes each row independently,
    /// collecting failures into [`BatchOutcome::skipped`].
    fn run_batch<T>(
        &self,
        rows: &[T],
        atomic: bool,
        write_one: impl Fn(&duckdb::Connection, &T, &str) -> Result<(), SiftError>,
    ) -> Result<BatchOutcome, SiftError> {
        let now = now_stamp();
        with_duckdb(&self.db_path, DuckAccess::Write, |conn| {
            if atomic {
                let tx = conn.unchecked_transaction().map_err(io)?;
                for (i, r) in rows.iter().enumerate() {
                    write_one(&tx, r, &now)
                        .map_err(|e| SiftError::Io(format!("row {}: {e}", i + 1)))?;
                }
                tx.commit().map_err(io)?;
                Ok(BatchOutcome {
                    written: rows.len(),
                    skipped: Vec::new(),
                })
            } else {
                let mut out = BatchOutcome::default();
                for (i, r) in rows.iter().enumerate() {
                    match write_one(conn, r, &now) {
                        Ok(()) => out.written += 1,
                        Err(e) => out.skipped.push((i + 1, format!("{e}"))),
                    }
                }
                Ok(out)
            }
        })
    }

    /// Every registered `std_key`. Used by the service layer to
    /// preflight `map set` before relying on the DuckDB foreign key.
    pub fn metric_keys(&self) -> Result<Vec<String>, SiftError> {
        let (_c, rows) = self.query("SELECT std_key FROM metrics")?;
        Ok(rows.into_iter().filter_map(|mut r| r.pop()).collect())
    }

    /// `(std_key, label, unit_kind)` rows, ordered — TSV round-trips
    /// back into `metric add`.
    pub fn list_metrics(&self) -> Result<(Vec<String>, Vec<Vec<String>>), SiftError> {
        self.query("SELECT std_key, label, unit_kind FROM metrics ORDER BY std_key")
    }

    /// `(source, raw_key, std_key)` rows, optionally filtered by
    /// source — TSV round-trips back into `map set`.
    pub fn list_map(
        &self,
        source: Option<&str>,
    ) -> Result<(Vec<String>, Vec<Vec<String>>), SiftError> {
        let (cols, mut rows) =
            self.query("SELECT source, raw_key, std_key FROM key_map ORDER BY source, raw_key")?;
        if let Some(s) = source {
            rows.retain(|r| r.first().map(|c| c == s).unwrap_or(false));
        }
        Ok((cols, rows))
    }

    /// Delete a metric. Refuses when `key_map` still references it
    /// unless `cascade` also removes those mappings.
    ///
    /// The mapping delete and the metric delete run in **separate**
    /// transactions on purpose: DuckDB cannot delete a parent and its
    /// FK-referencing child in one transaction (the FK index is not
    /// updated mid-transaction), so the child delete must be committed
    /// first. Returns the number of `metrics` rows deleted.
    pub fn delete_metric(&self, std_key: &str, cascade: bool) -> Result<usize, SiftError> {
        let refs: i64 = with_duckdb(&self.db_path, DuckAccess::Read, |conn| {
            let mut stmt = conn
                .prepare("SELECT count(*) FROM key_map WHERE std_key = ?")
                .map_err(io)?;
            let mut rows = stmt.query(duckdb::params![std_key]).map_err(io)?;
            let row = rows
                .next()
                .map_err(io)?
                .ok_or_else(|| SiftError::Io("count returned no row".into()))?;
            row.get(0).map_err(io)
        })?;
        if refs > 0 && !cascade {
            return Err(SiftError::Parse(format!(
                "metric {std_key:?} is referenced by {refs} mapping(s); delete them first or use --cascade"
            )));
        }
        if refs > 0 {
            // Commit the child delete before touching the parent.
            with_duckdb(&self.db_path, DuckAccess::Write, |conn| {
                conn.execute("DELETE FROM key_map WHERE std_key = ?", duckdb::params![std_key])
                    .map_err(io)
            })?;
        }
        with_duckdb(&self.db_path, DuckAccess::Write, |conn| {
            conn.execute("DELETE FROM metrics WHERE std_key = ?", duckdb::params![std_key])
                .map_err(io)
        })
    }

    /// Delete one `(source, raw_key)` mapping. Returns affected count.
    pub fn delete_map(&self, source: &str, raw_key: &str) -> Result<usize, SiftError> {
        with_duckdb(&self.db_path, DuckAccess::Write, |conn| {
            conn.execute(
                "DELETE FROM key_map WHERE source = ? AND raw_key = ?",
                duckdb::params![source, raw_key],
            )
            .map_err(io)
        })
    }

    /// Delete one fact row by primary key. Returns the affected count
    /// (0 if it did not exist).
    pub fn delete_fact(&self, k: &FactKey) -> Result<usize, SiftError> {
        with_duckdb(&self.db_path, DuckAccess::Write, |conn| {
            conn.execute(
                "DELETE FROM facts WHERE symbol=? AND fiscal_year=? AND period_type=? \
                 AND qmode=? AND scope=? AND raw_key=? AND source=?",
                duckdb::params![
                    k.symbol,
                    k.fiscal_year,
                    k.period_type.as_str(),
                    k.qmode.as_str(),
                    k.scope.as_str(),
                    k.raw_key,
                    k.source,
                ],
            )
            .map_err(io)
        })
    }

    /// Read-only query (`READ_ONLY` connection; any write is rejected
    /// by DuckDB). Returns `(column names, rows-of-strings)`.
    pub fn query(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<String>>), SiftError> {
        with_duckdb(&self.db_path, DuckAccess::Read, |conn| run_select(conn, sql))
    }

    /// Escape hatch: run arbitrary SQL under a writable connection.
    /// SELECT-shaped statements return [`SqlOutcome::Rows`]; DML/DDL
    /// return [`SqlOutcome::Affected`]. CHECK / FK / NOT NULL are still
    /// enforced by DuckDB, so this can delete and fix but cannot insert
    /// invalid data.
    pub fn execute(&self, sql: &str) -> Result<SqlOutcome, SiftError> {
        with_duckdb(&self.db_path, DuckAccess::Write, |conn| {
            if is_query_stmt(sql) {
                let (cols, rows) = run_select(conn, sql)?;
                Ok(SqlOutcome::Rows(cols, rows))
            } else {
                let n = conn.execute(sql, duckdb::params![]).map_err(io)?;
                Ok(SqlOutcome::Affected(n))
            }
        })
    }
}

/// Insert one fact + its `symbols` stub in the caller's connection /
/// transaction. The stub uses `name` (falling back to the symbol) and
/// derives `market` from the symbol's `.MARKET` suffix.
fn write_fact(conn: &duckdb::Connection, r: &FactRow, now: &str) -> Result<(), SiftError> {
    let market = market_of(&r.symbol).ok_or_else(|| {
        SiftError::Io(format!(
            "symbol {:?} lacks a .MARKET suffix (expected e.g. 600519.CN-A)",
            r.symbol
        ))
    })?;
    let stub_name = r.name.as_deref().unwrap_or(&r.symbol);
    conn.execute(
        "INSERT INTO symbols (symbol, name, market, created_at) VALUES (?,?,?,?) \
         ON CONFLICT DO NOTHING",
        duckdb::params![r.symbol, stub_name, market, now],
    )
    .map_err(io)?;

    let publish = r.publish_date.map(fmt_date);
    conn.execute(
        "INSERT INTO facts \
         (symbol, fiscal_year, period_type, qmode, scope, raw_key, source, value, currency, publish_date, created_at) \
         VALUES (?,?,?,?,?,?,?,?,?,?,?) \
         ON CONFLICT DO UPDATE SET \
           value = excluded.value, currency = excluded.currency, \
           publish_date = excluded.publish_date, created_at = excluded.created_at",
        duckdb::params![
            r.symbol,
            r.fiscal_year,
            r.period_type.as_str(),
            r.qmode.as_str(),
            r.scope.as_str(),
            r.raw_key,
            r.source,
            r.value,
            r.currency,
            publish,
            now,
        ],
    )
    .map_err(io)?;
    Ok(())
}

/// Upsert one metric definition. `_now` seeds `created_at` on insert.
fn write_metric(conn: &duckdb::Connection, r: &MetricRow, now: &str) -> Result<(), SiftError> {
    conn.execute(
        "INSERT INTO metrics (std_key, label, unit_kind, created_at) VALUES (?,?,?,?) \
         ON CONFLICT DO UPDATE SET label = excluded.label, unit_kind = excluded.unit_kind",
        duckdb::params![r.std_key, r.label, r.unit_kind, now],
    )
    .map_err(io)?;
    Ok(())
}

/// Upsert one mapping row. `key_map` has no `created_at`, so `now` is
/// unused here. The `std_key` FK to `metrics` is enforced by DuckDB.
fn write_map(conn: &duckdb::Connection, r: &MapRow, _now: &str) -> Result<(), SiftError> {
    conn.execute(
        "INSERT INTO key_map (source, raw_key, std_key) VALUES (?,?,?) \
         ON CONFLICT DO UPDATE SET std_key = excluded.std_key",
        duckdb::params![r.source, r.raw_key, r.std_key],
    )
    .map_err(io)?;
    Ok(())
}

/// Run a SELECT-shaped statement, stringifying every cell.
fn run_select(
    conn: &duckdb::Connection,
    sql: &str,
) -> Result<(Vec<String>, Vec<Vec<String>>), SiftError> {
    let mut stmt = conn.prepare(sql).map_err(io)?;
    let mut rows = stmt.query([]).map_err(io)?;
    let cols = rows
        .as_ref()
        .map(|s| s.column_names())
        .unwrap_or_default();
    let ncols = cols.len();
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(io)? {
        let mut r = Vec::with_capacity(ncols);
        for i in 0..ncols {
            let v: duckdb::types::Value = row.get(i).map_err(io)?;
            r.push(value_to_string(v));
        }
        out.push(r);
    }
    Ok((cols, out))
}

/// Heuristic: does `sql` produce a result set? Used only by the
/// escape hatch to pick print-vs-affected. Reads the first bareword.
fn is_query_stmt(sql: &str) -> bool {
    let head = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        head.as_str(),
        "select" | "with" | "from" | "values" | "show" | "describe" | "desc" | "explain"
            | "pragma" | "call" | "summarize" | "table"
    )
}

/// `{code}.{MARKET}` → `MARKET`. `None` when there is no dot suffix.
fn market_of(symbol: &str) -> Option<&str> {
    symbol.rsplit_once('.').map(|(_, m)| m)
}

fn fmt_date(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), d.month() as u8, d.day())
}

/// UTC `YYYY-MM-DD HH:MM:SS` — DuckDB parses this straight into
/// `TIMESTAMP` (no trailing `Z`, so it never becomes `TIMESTAMPTZ`).
fn now_stamp() -> String {
    stamp(OffsetDateTime::now_utc())
}

fn stamp(t: OffsetDateTime) -> String {
    let d = t.date();
    let tm = t.time();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        d.year(),
        d.month() as u8,
        d.day(),
        tm.hour(),
        tm.minute(),
        tm.second()
    )
}

/// Stringify a DuckDB cell per the f6 decision: NULL → empty string;
/// everything else → its natural text form. Dates / timestamps are
/// rendered human-readably (not raw epoch integers) via the `time`
/// crate.
fn value_to_string(v: duckdb::types::Value) -> String {
    use duckdb::types::{TimeUnit, Value};
    match v {
        Value::Null => String::new(),
        Value::Boolean(b) => b.to_string(),
        Value::TinyInt(n) => n.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::HugeInt(n) => n.to_string(),
        Value::UTinyInt(n) => n.to_string(),
        Value::USmallInt(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::UBigInt(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(f) => f.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Text(s) => s,
        Value::Enum(s) => s,
        Value::Date32(days) => Date::from_julian_day(days + UNIX_EPOCH_JULIAN_DAY)
            .map(fmt_date)
            .unwrap_or_else(|_| days.to_string()),
        Value::Timestamp(unit, v) => {
            let nanos = match unit {
                TimeUnit::Second => (v as i128) * 1_000_000_000,
                TimeUnit::Millisecond => (v as i128) * 1_000_000,
                TimeUnit::Microsecond => (v as i128) * 1_000,
                TimeUnit::Nanosecond => v as i128,
            };
            OffsetDateTime::from_unix_timestamp_nanos(nanos)
                .map(stamp)
                .unwrap_or_else(|_| v.to_string())
        }
        Value::Time64(unit, v) => {
            let secs = match unit {
                TimeUnit::Second => v,
                TimeUnit::Millisecond => v / 1_000,
                TimeUnit::Microsecond => v / 1_000_000,
                TimeUnit::Nanosecond => v / 1_000_000_000,
            };
            let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
            format!("{h:02}:{m:02}:{s:02}")
        }
        other => format!("{other:?}"),
    }
}

/// Julian day number of the Unix epoch (1970-01-01).
const UNIX_EPOCH_JULIAN_DAY: i32 = 2_440_588;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_store() -> (TempDir, FactStore) {
        let dir = TempDir::new().unwrap();
        let store = FactStore::open(dir.path().join("facts.duckdb")).unwrap();
        (dir, store)
    }

    fn fact(symbol: &str, raw_key: &str, value: f64) -> FactRow {
        FactRow {
            symbol: symbol.into(),
            fiscal_year: 2024,
            period_type: PeriodType::Annual,
            qmode: QMode::Na,
            scope: Scope::Na,
            raw_key: raw_key.into(),
            source: "manual".into(),
            value,
            currency: None,
            publish_date: None,
            name: None,
        }
    }

    #[test]
    fn open_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("nested").join("facts.duckdb");
        let _a = FactStore::open(p.clone()).unwrap();
        let _b = FactStore::open(p).unwrap();
    }

    #[test]
    fn upsert_then_query_round_trips_and_stubs_symbol() {
        let (_d, s) = temp_store();
        let out = s
            .upsert_facts(&[fact("600519.CN-A", "employee_comp", 1.5e9)], true)
            .unwrap();
        assert_eq!(out.written, 1);
        let (cols, rows) = s
            .query("SELECT value FROM facts WHERE raw_key='employee_comp'")
            .unwrap();
        assert_eq!(cols, vec!["value"]);
        // Rust's f64 Display drops a `.0` on whole numbers.
        assert_eq!(rows, vec![vec!["1500000000".to_string()]]);
        // Symbol stub auto-created, FK satisfied, name defaults to symbol.
        let (_c, srows) = s
            .query("SELECT market, name FROM symbols WHERE symbol='600519.CN-A'")
            .unwrap();
        assert_eq!(srows, vec![vec!["CN-A".to_string(), "600519.CN-A".to_string()]]);
    }

    #[test]
    fn upsert_overwrites_same_primary_key() {
        let (_d, s) = temp_store();
        s.upsert_facts(&[fact("600519.CN-A", "k", 1.0)], true).unwrap();
        s.upsert_facts(&[fact("600519.CN-A", "k", 2.0)], true).unwrap();
        let (_c, rows) = s.query("SELECT value FROM facts WHERE raw_key='k'").unwrap();
        assert_eq!(rows, vec![vec!["2".to_string()]]);
    }

    #[test]
    fn atomic_batch_rolls_back_on_bad_row() {
        let (_d, s) = temp_store();
        let mut bad = fact("600519.CN-A", "k", 1.0);
        bad.fiscal_year = 1800; // violates CHECK(fiscal_year BETWEEN 1990 AND 2100)
        let rows = vec![fact("600519.CN-A", "ok", 1.0), bad];
        let err = s.upsert_facts(&rows, true).unwrap_err();
        assert!(format!("{err}").contains("row 2"), "err: {err}");
        // Nothing written — the good first row rolled back too.
        let (_c, r) = s.query("SELECT count(*) FROM facts").unwrap();
        assert_eq!(r, vec![vec!["0".to_string()]]);
    }

    #[test]
    fn skip_invalid_writes_good_rows_and_reports_bad() {
        let (_d, s) = temp_store();
        let mut bad = fact("600519.CN-A", "k", 1.0);
        bad.fiscal_year = 1800;
        let rows = vec![
            fact("600519.CN-A", "a", 1.0),
            bad,
            fact("600519.CN-A", "b", 2.0),
        ];
        let out = s.upsert_facts(&rows, false).unwrap();
        assert_eq!(out.written, 2);
        assert_eq!(out.skipped.len(), 1);
        assert_eq!(out.skipped[0].0, 2);
    }

    #[test]
    fn cross_column_check_rejects_single_on_annual() {
        let (_d, s) = temp_store();
        let mut r = fact("600519.CN-A", "k", 1.0);
        r.qmode = QMode::Single;
        r.period_type = PeriodType::Annual; // single requires q1..q4
        assert!(s.upsert_facts(&[r], true).is_err());
    }

    #[test]
    fn v_facts_computes_period_and_falls_back_to_raw_key() {
        let (_d, s) = temp_store();
        let mut r = fact("600519.CN-A", "TOTAL_OPERATE_INCOME", 42.0);
        r.period_type = PeriodType::Q3;
        r.qmode = QMode::Cumulative;
        s.upsert_facts(&[r], true).unwrap();
        let (_c, rows) = s
            .query("SELECT period, period_end, key, mapped FROM v_facts")
            .unwrap();
        assert_eq!(
            rows,
            vec![vec![
                "2024Q3".to_string(),
                "2024-09-30".to_string(),
                "TOTAL_OPERATE_INCOME".to_string(),
                "false".to_string(),
            ]]
        );
    }

    #[test]
    fn query_is_read_only() {
        let (_d, s) = temp_store();
        s.upsert_facts(&[fact("600519.CN-A", "k", 1.0)], true).unwrap();
        assert!(s.query("DELETE FROM facts").is_err());
    }

    #[test]
    fn execute_deletes_and_reports_affected() {
        let (_d, s) = temp_store();
        s.upsert_facts(
            &[
                fact("600519.CN-A", "a", 1.0),
                fact("600519.CN-A", "b", 2.0),
            ],
            true,
        )
        .unwrap();
        let out = s.execute("DELETE FROM facts WHERE raw_key='a'").unwrap();
        assert_eq!(out, SqlOutcome::Affected(1));
    }

    #[test]
    fn execute_select_returns_rows() {
        let (_d, s) = temp_store();
        s.upsert_facts(&[fact("600519.CN-A", "k", 7.0)], true).unwrap();
        match s.execute("SELECT value FROM facts").unwrap() {
            SqlOutcome::Rows(cols, rows) => {
                assert_eq!(cols, vec!["value"]);
                assert_eq!(rows, vec![vec!["7".to_string()]]);
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn execute_rejects_invalid_data_even_via_hatch() {
        let (_d, s) = temp_store();
        s.upsert_facts(&[fact("600519.CN-A", "k", 1.0)], true).unwrap();
        // Escape hatch can still not bypass the CHECK constraint.
        let err = s
            .execute("INSERT INTO facts VALUES ('600519.CN-A',2024,'ytd','na','na','k','manual',1.0,NULL,NULL,now())")
            .unwrap_err();
        let _ = err; // any error is fine — the point is it does not succeed
    }

    fn metric(std_key: &str, unit_kind: &str) -> MetricRow {
        MetricRow {
            std_key: std_key.into(),
            label: None,
            unit_kind: unit_kind.into(),
        }
    }
    fn mapping(source: &str, raw_key: &str, std_key: &str) -> MapRow {
        MapRow {
            source: source.into(),
            raw_key: raw_key.into(),
            std_key: std_key.into(),
        }
    }

    #[test]
    fn metrics_upsert_list_and_bad_unit_kind_rejected() {
        let (_d, s) = temp_store();
        s.upsert_metrics(&[metric("revenue", "amount")], true).unwrap();
        let (cols, rows) = s.list_metrics().unwrap();
        assert_eq!(cols, vec!["std_key", "label", "unit_kind"]);
        assert_eq!(rows, vec![vec!["revenue".to_string(), String::new(), "amount".to_string()]]);
        // CHECK rejects an out-of-whitelist unit_kind.
        assert!(s.upsert_metrics(&[metric("roe", "percentage")], true).is_err());
    }

    #[test]
    fn map_fk_requires_registered_std_key() {
        let (_d, s) = temp_store();
        // No metric yet → FK violation.
        assert!(s
            .upsert_map(&[mapping("eastmoney", "TOTAL_OPERATE_INCOME", "revenue")], true)
            .is_err());
        s.upsert_metrics(&[metric("revenue", "amount")], true).unwrap();
        s.upsert_map(&[mapping("eastmoney", "TOTAL_OPERATE_INCOME", "revenue")], true)
            .unwrap();
        assert_eq!(s.metric_keys().unwrap(), vec!["revenue".to_string()]);
    }

    #[test]
    fn delete_metric_refuses_referenced_then_cascades() {
        let (_d, s) = temp_store();
        s.upsert_metrics(&[metric("revenue", "amount")], true).unwrap();
        s.upsert_map(&[mapping("eastmoney", "TOTAL_OPERATE_INCOME", "revenue")], true)
            .unwrap();
        // Referenced → refused without cascade.
        assert!(s.delete_metric("revenue", false).is_err());
        // Cascade removes the mapping (separate txn) then the metric.
        assert_eq!(s.delete_metric("revenue", true).unwrap(), 1);
        assert!(s.metric_keys().unwrap().is_empty());
        assert!(s.list_map(None).unwrap().1.is_empty());
    }

    #[test]
    fn mapped_key_reflects_in_v_facts() {
        let (_d, s) = temp_store();
        let mut r = fact("600519.CN-A", "TOTAL_OPERATE_INCOME", 1.0);
        r.source = "eastmoney".into();
        s.upsert_facts(&[r], true).unwrap();
        s.upsert_metrics(&[metric("revenue", "amount")], true).unwrap();
        s.upsert_map(&[mapping("eastmoney", "TOTAL_OPERATE_INCOME", "revenue")], true)
            .unwrap();
        let (_c, rows) = s
            .query("SELECT key, mapped FROM v_facts WHERE symbol='600519.CN-A'")
            .unwrap();
        assert_eq!(rows, vec![vec!["revenue".to_string(), "true".to_string()]]);
    }

    #[test]
    fn delete_fact_by_key() {
        let (_d, s) = temp_store();
        s.upsert_facts(&[fact("600519.CN-A", "k", 1.0)], true).unwrap();
        let n = s
            .delete_fact(&FactKey {
                symbol: "600519.CN-A".into(),
                fiscal_year: 2024,
                period_type: PeriodType::Annual,
                qmode: QMode::Na,
                scope: Scope::Na,
                raw_key: "k".into(),
                source: "manual".into(),
            })
            .unwrap();
        assert_eq!(n, 1);
    }
}
