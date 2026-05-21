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
//! - No TTL: announcement metadata is essentially immutable after
//!   publication, so cached rows live forever. A future `Kind` that
//!   *does* need expiry can add a `ttl_seconds` column with `ALTER
//!   TABLE`.
//! - Keys are `blake3(kind_label : scope_parts? : id)` to keep the
//!   namespace flat while letting two `Kind`s share an id without
//!   collision.
//! - Init failure degrades to a `no_cache` instance: reads always
//!   miss, writes are dropped. The CLI must not block on a broken
//!   cache.
//! - Lock contention from a sibling sift process is handled by
//!   [`open_db_retrying`]'s `20 / 80 / 320 ms` backoff (same ladder
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
use std::sync::OnceLock;

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
    /// `domain::AnnouncementRow`.
    AnnounceMeta,
}

impl Kind {
    pub const fn label(self) -> &'static str {
        match self {
            Kind::AnnounceMeta => "announce-meta",
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

/// Default on-disk location: `~/.sift/cache/records.duckdb`. Tests
/// pass an explicit path through [`RecordCache::open_at`].
pub fn default_path() -> Result<PathBuf, SiftError> {
    Ok(super::cache_root()?.join("records.duckdb"))
}

/// Process-shared blob-by-key cache. Lightweight — just the on-disk
/// path; connections are opened per call by [`open_db_retrying`].
/// Cheap to clone, `Send + Sync` without any interior locking.
pub struct RecordCache {
    db_path: PathBuf,
    /// When `true`, every `get` returns `None` and every `put` is a
    /// no-op. Set by [`open_or_fallback`] on init failure so the CLI
    /// keeps running with the cache effectively disabled.
    no_cache: bool,
}

/// One hit's payload. Body only — there is no TTL gate today, and
/// the row's write timestamp lives in the DB row but is not exposed
/// here because no current caller needs it. Add `cached_at` back if
/// a future feature surfaces "cached N hours ago" or similar.
pub struct CacheEntry {
    pub body: Vec<u8>,
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
    /// Open (or create) the cache at the default path.
    pub fn open() -> Result<Self, SiftError> {
        Self::open_at(&default_path()?)
    }

    /// Test seam: open at an explicit path. Production callers use
    /// [`open`] / [`open_or_fallback`]. Opens a connection just long
    /// enough to run `CREATE TABLE IF NOT EXISTS`, then drops it; the
    /// returned struct holds only the path. Subsequent `get` / `put`
    /// each open a fresh connection.
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
            no_cache: false,
        })
    }

    /// Process-shared singleton. Caches the resolved path + the
    /// one-shot schema-init result so repeated `shared()` calls
    /// within one CLI invocation don't re-`mkdir` + re-`CREATE TABLE`.
    /// The connections themselves are still opened per `get`/`put`
    /// — see the module docs for why.
    ///
    /// Integration tests sandbox `$HOME` (see `tests/*_e2e.rs`), so
    /// the singleton path lands on a tempdir there. Unit tests that
    /// need explicit cache control still call
    /// [`RecordCache::open_at`].
    pub fn shared() -> &'static RecordCache {
        static SHARED: OnceLock<RecordCache> = OnceLock::new();
        SHARED.get_or_init(RecordCache::open_or_fallback)
    }

    /// Init-failure-safe constructor. Logs the underlying error to
    /// stderr once and returns a no-op cache. Used internally by
    /// [`shared`]; production code should prefer the singleton.
    ///
    /// Transient lock contention from a sibling sift process is
    /// already absorbed by [`open_db_retrying`]'s 420ms backoff
    /// ladder, so by the time we get an `Err` here it is something
    /// the user should know about — permissions, disk full, schema
    /// corruption — not benign pipeline contention.
    pub fn open_or_fallback() -> Self {
        match Self::open() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[warn] record cache unavailable: {e}; running without cache");
                Self::no_op()
            }
        }
    }

    /// Construct a no-op cache (every read misses, every write
    /// dropped). Internal — used by [`open_or_fallback`] when init
    /// fails. The path field is a placeholder; with `no_cache=true`
    /// it is never touched.
    fn no_op() -> Self {
        Self {
            db_path: PathBuf::new(),
            no_cache: true,
        }
    }

    /// Look up an entry. Returns `None` on miss, on the no-op
    /// fallback, or on any DB error (errors are silent — the caller
    /// will just fall through to the upstream path). Opens and
    /// drops a connection for the single SELECT.
    pub fn get(&self, kind: Kind, scope: &[&str], id: &str) -> Option<CacheEntry> {
        if self.no_cache {
            return None;
        }
        let key = make_key(kind, scope, id);
        super::with_duckdb(&self.db_path, super::DuckAccess::Read, |conn| {
            let mut stmt = conn
                .prepare("SELECT body FROM cache_entries WHERE key = ?")
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            let mut iter = stmt
                .query_map([&key as &dyn duckdb::ToSql], |row| {
                    let body: Vec<u8> = row.get(0)?;
                    Ok(body)
                })
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            // First (and only) row: Some(Ok(body)). Anything else → miss.
            let body = match iter.next() {
                Some(Ok(b)) => b,
                _ => return Ok(None),
            };
            Ok(Some(CacheEntry { body }))
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
    pub fn put_many<I>(&self, kind: Kind, scope: &[&str], items: I)
    where
        I: IntoIterator<Item = (String, Vec<u8>)>,
    {
        if self.no_cache {
            return;
        }
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

    #[test]
    fn no_op_cache_silently_drops_writes_and_always_misses() {
        let cache = RecordCache::no_op();
        cache.put(Kind::AnnounceMeta, &[], "x", b"body");
        assert!(cache.get(Kind::AnnounceMeta, &[], "x").is_none());
    }

    #[test]
    fn shared_returns_same_instance_across_calls() {
        // Two consecutive `shared()` calls return the same `&'static`
        // reference — the OnceLock fires init once and reuses the
        // same handle. (Address equality is the cheapest proof.)
        let a = RecordCache::shared();
        let b = RecordCache::shared();
        assert!(
            std::ptr::eq(a as *const _, b as *const _),
            "shared() must return the same instance on repeated calls"
        );
    }
}
