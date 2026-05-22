//! Filesystem blob-by-name cache.
//!
//! Counterpart to [`crate::cache::record::RecordCache`] (DuckDB
//! blob-by-key). Two callers live on top of it:
//!
//! - cninfo listings: cached JSON envelopes under `cninfo/`, mtime TTL
//!   (orchestrator: `fetch::search`).
//! - Announcement PDF binaries: PDFs under `announcements/`, no TTL
//!   (orchestrator: `fetch::announce`).
//!
//! `FileCache` exposes file-level primitives keyed by a relative
//! subpath (e.g. `"cninfo/szse_stock.json"`,
//! `"announcements/<id>.pdf"`); each feature encodes its own key
//! convention. The struct itself is just a `PathBuf` root — no I/O at
//! construction, no schema, no validation. `AppContext::file_cache`
//! is `None` when `$HOME` couldn't be resolved at startup; every
//! caller already guards on the Option.
//!
//! ## Why a struct over free functions
//!
//! Hoisting `cache::{atomic_write, is_fresh, format_mtime_utc}` onto
//! a single type lets `AppContext` carry one Option per cache flavor
//! (`file` vs `record`) and gives unit tests an injection point that
//! doesn't depend on `$HOME` or env var monkey-patching.

use std::path::PathBuf;

use crate::error::SiftError;

/// Filesystem blob-by-name cache rooted at a directory. Holds only
/// the path; every operation resolves a subpath at call time. Cheap
/// to clone, `Send + Sync` without interior locking.
pub struct FileCache {
    root: PathBuf,
}

impl FileCache {
    /// Construct a cache rooted at `root`. No I/O, no validation —
    /// the directory may not exist yet. `write` calls `mkdir -p` on
    /// the parent before the actual write.
    pub fn open(root: PathBuf) -> Self {
        Self { root }
    }

    /// Resolve `<root>/<key>`. Does not check existence.
    pub fn path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    /// Read the entire body. `None` on missing file, IO error, or
    /// permissions issue. Callers that need to distinguish those
    /// cases should reach for `std::fs` directly.
    pub fn read(&self, key: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path(key)).ok()
    }

    /// `true` iff the entry exists with non-zero size. A zero-byte
    /// file is treated as "not cached" — defense against
    /// half-written downloads that escaped the atomic-write path.
    pub fn exists(&self, key: &str) -> bool {
        std::fs::metadata(self.path(key))
            .map(|m| m.is_file() && m.len() > 0)
            .unwrap_or(false)
    }

    /// `true` iff the entry exists and its mtime is within
    /// `ttl_secs` of now. All error cases (missing file, unsupported
    /// platform, clock skew) degrade to `false` — at worst the
    /// caller pays one extra fetch.
    pub fn is_fresh(&self, key: &str, ttl_secs: u64) -> bool {
        super::is_fresh(&self.path(key), ttl_secs)
    }

    /// Atomic write: sibling `.tmp` + `rename`. Creates missing
    /// parent dirs. On failure the destination is untouched and no
    /// `.tmp` lingers.
    pub fn write(&self, key: &str, bytes: &[u8]) -> Result<(), SiftError> {
        super::atomic_write(&self.path(key), bytes)
    }

    /// Format the entry's mtime as `YYYY-MM-DD HH:MM:SS` UTC. Falls
    /// back to a literal `"?"` for any failure (missing, no mtime,
    /// pre-epoch). Used in stale-fallback warnings — never panics.
    pub fn mtime_str(&self, key: &str) -> String {
        super::format_mtime_utc(&self.path(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    fn make() -> (TempDir, FileCache) {
        let tmp = TempDir::new().unwrap();
        let cache = FileCache::open(tmp.path().to_path_buf());
        (tmp, cache)
    }

    #[test]
    fn write_then_read_round_trips_bytes() {
        let (_tmp, cache) = make();
        cache.write("cninfo/szse_stock.json", b"{\"stockList\":[]}").unwrap();
        let bytes = cache.read("cninfo/szse_stock.json").unwrap();
        assert_eq!(bytes, b"{\"stockList\":[]}");
    }

    #[test]
    fn read_returns_none_for_missing_file() {
        let (_tmp, cache) = make();
        assert!(cache.read("does/not/exist.json").is_none());
    }

    #[test]
    fn exists_false_for_missing_file() {
        let (_tmp, cache) = make();
        assert!(!cache.exists("announcements/123.pdf"));
    }

    #[test]
    fn exists_false_for_zero_byte_file() {
        let (_tmp, cache) = make();
        let p = cache.path("announcements/empty.pdf");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"").unwrap();
        assert!(!cache.exists("announcements/empty.pdf"));
    }

    #[test]
    fn exists_true_for_non_empty_file_after_write() {
        let (_tmp, cache) = make();
        cache.write("announcements/abc.pdf", b"%PDF-1.4").unwrap();
        assert!(cache.exists("announcements/abc.pdf"));
    }

    #[test]
    fn write_creates_parent_dirs_recursively() {
        let (_tmp, cache) = make();
        // Two levels of missing parent.
        cache.write("a/b/c/d.txt", b"hello").unwrap();
        assert!(cache.path("a/b/c").is_dir());
        assert_eq!(cache.read("a/b/c/d.txt").unwrap(), b"hello");
    }

    #[test]
    fn write_overwrites_existing_file() {
        let (_tmp, cache) = make();
        cache.write("x", b"old").unwrap();
        cache.write("x", b"new").unwrap();
        assert_eq!(cache.read("x").unwrap(), b"new");
    }

    #[test]
    fn is_fresh_true_for_just_written_file() {
        let (_tmp, cache) = make();
        cache.write("k", b"x").unwrap();
        assert!(cache.is_fresh("k", 3600));
    }

    #[test]
    fn is_fresh_false_for_expired_mtime() {
        let (_tmp, cache) = make();
        cache.write("k", b"x").unwrap();
        let past = SystemTime::now() - Duration::from_secs(25 * 3600);
        filetime::set_file_mtime(
            cache.path("k"),
            filetime::FileTime::from_system_time(past),
        )
        .unwrap();
        assert!(!cache.is_fresh("k", 24 * 3600));
    }

    #[test]
    fn is_fresh_false_for_missing_file() {
        let (_tmp, cache) = make();
        assert!(!cache.is_fresh("nope", 3600));
    }

    #[test]
    fn mtime_str_returns_19_char_timestamp_for_existing_file() {
        let (_tmp, cache) = make();
        cache.write("k", b"x").unwrap();
        let s = cache.mtime_str("k");
        assert_eq!(s.len(), 19, "expected YYYY-MM-DD HH:MM:SS, got {s:?}");
        assert_ne!(s, "?", "should resolve a real timestamp");
    }

    #[test]
    fn mtime_str_returns_question_mark_for_missing_file() {
        let (_tmp, cache) = make();
        assert_eq!(cache.mtime_str("nope"), "?");
    }

    #[test]
    fn path_does_not_validate_existence() {
        let (tmp, cache) = make();
        let p = cache.path("anything/that/does/not/exist");
        assert!(p.starts_with(tmp.path()));
        // No fs::* call — file does not need to exist for `path` to return.
    }
}
