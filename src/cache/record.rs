//! Generic per-record blob-by-key cache backed by DuckDB.
//!
//! Pattern adopted from `bio-ncbi`'s `record_cache.rs` — the same
//! `cache_entries(key BLOB PK, body BLOB, …)` table shape and the
//! same **open-per-call** connection lifetime. We do not hold a
//! long-lived `duckdb::Connection`: every `get` / `put` opens a fresh
//! connection, does its op, then drops it. This is the only way to
//! play nicely with multi-process pipelines like
//! `sift announce list … | sift announce show …` — DuckDB takes an
//! OS-level exclusive file lock at connection open
//! (<https://duckdb.org/docs/stable/connect/concurrency>) so a
//! singleton held for the whole CLI invocation locks out the sibling
//! process.
//!
//! ## Persistence discipline
//! - Two `Kind`s share this table today:
//!   - `Kind::AnnounceMeta` — F3 announcement rows. Immutable after
//!     publication, so cached entries live forever; every consumer
//!     ignores `created_at`.
//!   - `Kind::Financials` — F2 financial-row groups. Caller applies
//!     a TTL policy on top of [`CacheEntry::created_at`] —
//!     `RecordCache` itself stays TTL-agnostic.
//! - Keys are `blake3(kind_label : scope_parts? : id)` to keep the
//!   namespace flat while letting two `Kind`s share an id without
//!   collision.
//! - Init failure is reported to `main.rs`, which warns the user and
//!   sets [`crate::app::AppContext::records_cache`] to `None`; every
//!   call site that touches the cache already guards on the Option.
//!   The CLI must not block on a broken cache.
//! - Lock contention from a sibling sift process is handled by
//!   `with_duckdb`'s `20 / 80 / 320 ms` backoff (same ladder
//!   bio-ncbi uses): typical writes hold the lock for ~1 ms, so a
//!   reader on the other end of a pipe almost always slots into a
//!   gap.
//!
//! ## Batching
//! For tight write loops (e.g. `cache_rows_by_id(&[N rows])` from the
//! announce paginate fallback), prefer [`RecordCache::put_many`] over
//! N calls to [`RecordCache::put`]: a single connection serves the
//! whole batch, so the per-call open overhead is paid once per page,
//! not once per row.

use std::path::{Path, PathBuf};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::error::SiftError;

/// Which family a cache entry belongs to. Drives the key namespace
/// (so two endpoints can use the same id without collision) and the
/// `kind` column for debugging. Forward-compatible: adding a variant
/// is a one-line change + a `label` arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// cninfo announcement metadata, keyed by `announcementId`.
    /// One row per announcement; body is the JSON-serialized
    /// `domain::AnnouncementRow`. No TTL — announcement metadata is
    /// immutable after publication.
    AnnounceMeta,
    /// F2 financial-row group, keyed on
    /// `(symbol, market, statement, scope, source) → period_iso`.
    /// Body is JSON-serialized `Vec<StoredFinancialRow>` (one row per
    /// item within that period). TTL is applied by the caller in
    /// `fetch::report` against [`CacheEntry::created_at`].
    Financials,
}

impl Kind {
    pub const fn label(self) -> &'static str {
        match self {
            Kind::AnnounceMeta => "announce-meta",
            Kind::Financials => "financials",
        }
    }
}

/// Build a content-addressed key. `scope_parts` is empty when the id
/// is already globally unique (e.g. cninfo `announcementId`); add
/// scope when the id is only unique within a context (e.g.
/// `(db, uid)` for an NCBI esummary).
pub fn make_key(kind: Kind, scope_parts: &[&str], id: &str) -> Vec<u8> {
    let mut h = blake3::Hasher::new();
    h.update(kind.label().as_bytes());
    for s in scope_parts {
        h.update(b":");
        h.update(s.as_bytes());
    }
    h.update(b":");
    h.update(id.as_bytes());
    h.finalize().as_bytes().to_vec()
}

/// Process-shared blob-by-key cache. Lightweight — just the on-disk
/// path; connections are opened per call by `with_duckdb`. Cheap to
/// hold by value, `Send + Sync` without any interior locking.
pub struct RecordCache {
    db_path: PathBuf,
}

/// One hit's payload. `RecordCache` itself does not enforce TTL —
/// callers that need it (F2 financials) apply their bucket policy on
/// top of [`Self::created_at`]; callers that don't (F3 announce
/// metadata) ignore the field.
pub struct CacheEntry {
    pub body: Vec<u8>,
    /// Wall-clock instant the row was last written. Sourced from the
    /// `created_at TEXT` column (RFC 3339); a malformed timestamp on
    /// disk surfaces here as `None` and the caller treats the entry
    /// as if the row were missing.
    pub created_at: Option<OffsetDateTime>,
}

const SCHEMA_SQL: &str = "
    CREATE TABLE IF NOT EXISTS cache_entries (
        key        BLOB PRIMARY KEY,
        endpoint   TEXT NOT NULL,
        body       BLOB NOT NULL,
        status     INTEGER NOT NULL DEFAULT 200,
        created_at TEXT NOT NULL,
        kind       TEXT NOT NULL
    );
";

impl RecordCache {
    /// Open at an explicit path. Production callers compose
    /// `ctx.file_cache.path("records.duckdb")`; tests pass a tempdir
    /// path. Opens a connection just long enough to run
    /// `CREATE TABLE IF NOT EXISTS`, then drops it; the returned
    /// struct holds only the path. Subsequent `get` / `put` each open
    /// a fresh connection.
    pub fn open_at(path: &Path) -> Result<Self, SiftError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SiftError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }
        super::with_duckdb(path, super::DuckAccess::Write, |conn| {
            conn.execute_batch(SCHEMA_SQL)
                .map_err(|e| SiftError::Io(format!("record cache schema: {e}")))
        })?;
        Ok(Self {
            db_path: path.to_path_buf(),
        })
    }

    /// Look up an entry. Returns `None` on miss or on any DB error
    /// (errors are silent — the caller falls through to the upstream
    /// path). Opens and drops a connection for the single SELECT.
    pub fn get(&self, kind: Kind, scope: &[&str], id: &str) -> Option<CacheEntry> {
        let key = make_key(kind, scope, id);
        super::with_duckdb(&self.db_path, super::DuckAccess::Read, |conn| {
            let mut stmt = conn
                .prepare("SELECT body, created_at FROM cache_entries WHERE key = ?")
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            let mut iter = stmt
                .query_map([&key as &dyn duckdb::ToSql], |row| {
                    let body: Vec<u8> = row.get(0)?;
                    let created_at_s: String = row.get(1)?;
                    Ok((body, created_at_s))
                })
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            // First (and only) row: Some(Ok((body, created_at))). Anything else → miss.
            let (body, created_at_s) = match iter.next() {
                Some(Ok(pair)) => pair,
                _ => return Ok(None),
            };
            let created_at = OffsetDateTime::parse(&created_at_s, &Rfc3339).ok();
            Ok(Some(CacheEntry { body, created_at }))
        })
        .ok()
        .flatten()
    }

    /// Upsert one record. Opens and drops a connection for the
    /// single INSERT. For tight write loops use [`put_many`] instead
    /// — it shares one connection across the whole batch.
    pub fn put(&self, kind: Kind, scope: &[&str], id: &str, body: &[u8]) {
        self.put_many(kind, scope, std::iter::once((id.to_string(), body.to_vec())));
    }

    /// Upsert a batch of records under one shared connection. The
    /// per-call open cost is paid once per batch instead of once per
    /// row, which matters when the announce paginate fallback fires
    /// `put_many` per page (~30 rows). Cache errors only log to
    /// stderr; a failed write must never abort the user-facing path.
    /// Test helper: rewrite the `created_at` of an existing entry so
    /// fetch-layer TTL tests can simulate stale rows without sleeping.
    #[cfg(test)]
    pub(crate) fn force_created_at(
        &self,
        kind: Kind,
        scope: &[&str],
        id: &str,
        when: OffsetDateTime,
    ) {
        let key = make_key(kind, scope, id);
        let when_s = when.format(&Rfc3339).unwrap_or_default();
        let _ = super::with_duckdb(&self.db_path, super::DuckAccess::Write, |conn| {
            conn.execute(
                "UPDATE cache_entries SET created_at = ? WHERE key = ?",
                duckdb::params![&when_s, &key],
            )
            .map_err(|e| SiftError::Io(format!("force_created_at: {e}")))?;
            Ok(())
        });
    }

    pub fn put_many<I>(&self, kind: Kind, scope: &[&str], items: I)
    where
        I: IntoIterator<Item = (String, Vec<u8>)>,
    {
        let label = kind.label();
        // `with_duckdb` already handles retry / drop / lock release.
        // Inside the closure we keep the prepared statement alive for
        // the whole batch so DuckDB only parses the SQL once.
        let res = super::with_duckdb(&self.db_path, super::DuckAccess::Write, |conn| {
            let mut stmt = conn
                .prepare(
                    "INSERT INTO cache_entries (key, endpoint, body, status, created_at, kind)
                     VALUES (?, ?, ?, 200, ?, ?)
                     ON CONFLICT(key) DO UPDATE SET
                         body = excluded.body,
                         status = excluded.status,
                         created_at = excluded.created_at",
                )
                .map_err(|e| SiftError::Io(format!("prepare: {e}")))?;
            let now = OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_default();
            for (id, body) in items {
                let key = make_key(kind, scope, &id);
                if let Err(e) =
                    stmt.execute(duckdb::params![key, label, body, now, label])
                {
                    eprintln!("[warn] record cache write failed ({label}/{id}): {e}");
                }
            }
            Ok(())
        });
        if let Err(e) = res {
            eprintln!("[warn] record cache write skipped ({label}): {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_cache() -> (TempDir, RecordCache) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test-records.duckdb");
        let cache = RecordCache::open_at(&path).unwrap();
        (dir, cache)
    }

    #[test]
    fn open_is_idempotent_and_creates_schema() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("records.duckdb");
        // Parent dir auto-created.
        let _c1 = RecordCache::open_at(&path).unwrap();
        // Re-open same file: schema CREATE IF NOT EXISTS keeps it sane.
        let _c2 = RecordCache::open_at(&path).unwrap();
    }

    #[test]
    fn put_then_get_round_trips_body() {
        let (_dir, cache) = temp_cache();
        cache.put(Kind::AnnounceMeta, &[], "1219506510", b"{\"id\":\"1219506510\"}");
        let hit = cache.get(Kind::AnnounceMeta, &[], "1219506510").unwrap();
        assert_eq!(hit.body, b"{\"id\":\"1219506510\"}");
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let (_dir, cache) = temp_cache();
        assert!(cache.get(Kind::AnnounceMeta, &[], "no-such-id").is_none());
    }

    #[test]
    fn put_many_writes_all_rows_under_one_connection() {
        let (_dir, cache) = temp_cache();
        cache.put_many(
            Kind::AnnounceMeta,
            &[],
            (0..5).map(|i| (format!("id-{i}"), format!("body-{i}").into_bytes())),
        );
        for i in 0..5 {
            let id = format!("id-{i}");
            let want = format!("body-{i}").into_bytes();
            let hit = cache
                .get(Kind::AnnounceMeta, &[], &id)
                .unwrap_or_else(|| panic!("expected hit for {id}"));
            assert_eq!(hit.body, want);
        }
    }

    #[test]
    fn put_many_with_empty_iterator_is_noop() {
        let (_dir, cache) = temp_cache();
        cache.put_many(Kind::AnnounceMeta, &[], std::iter::empty());
        assert!(cache.get(Kind::AnnounceMeta, &[], "x").is_none());
    }

    #[test]
    fn put_overwrites_existing_entry() {
        let (_dir, cache) = temp_cache();
        cache.put(Kind::AnnounceMeta, &[], "x", b"first");
        cache.put(Kind::AnnounceMeta, &[], "x", b"second");
        let hit = cache.get(Kind::AnnounceMeta, &[], "x").unwrap();
        assert_eq!(hit.body, b"second");
    }

    #[test]
    fn make_key_is_deterministic_and_scope_sensitive() {
        let a = make_key(Kind::AnnounceMeta, &[], "1");
        let b = make_key(Kind::AnnounceMeta, &[], "1");
        assert_eq!(a, b, "same inputs → same key");
        let c = make_key(Kind::AnnounceMeta, &["scope"], "1");
        assert_ne!(a, c, "scope changes the key");
    }

}
