//! Filesystem cache primitives shared by every cached command.
//!
//! - [`cache_root`] resolves `~/.sift/cache` — the path is fixed by
//!   README decision and intentionally not overridable.
//! - [`is_fresh`] compares a file's mtime against a TTL.
//! - [`atomic_write`] writes via a sibling `.tmp` + `rename` so a
//!   reader never sees a half-written body.
//! - [`format_mtime_utc`] / [`format_utc_secs`] render `mtime` as a
//!   `YYYY-MM-DD HH:MM:SS` UTC string for stale-cache warnings,
//!   without pulling a date crate.

pub mod announcements;
pub mod financials;
pub mod record;
pub mod search;
pub mod ttl;

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use crate::error::SiftError;

/// 24 h. Matches the F1 README "缓存策略" decision.
pub const CACHE_TTL_SEARCH_SECS: u64 = 24 * 3600;

// ---------------------------------------------------------------------------
// DuckDB access — the one approved way to touch a `.duckdb` cache file
// ---------------------------------------------------------------------------

/// Connection intent for [`with_duckdb`]. Today both modes share the
/// same retry-on-lock strategy in `open_duckdb_retrying`, but the
/// label drives DuckDB's `ACCESS_MODE` setting (so a misuse like
/// running `INSERT` under [`DuckAccess::Read`] is rejected by the DB
/// instead of silently corrupting state) and serves as inline
/// documentation at every call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DuckAccess {
    /// `ACCESS_MODE = READ_ONLY`. With DuckDB's current single-writer
    /// file-lock model a reader still loses if a writer holds the
    /// exclusive lock, so reads also block via the retry ladder; the
    /// label is for self-documentation and DB-enforced write-rejection.
    Read,
    /// `ACCESS_MODE = READ_WRITE` (DuckDB's default). Takes the
    /// exclusive file lock; the retry ladder absorbs contention from
    /// any sibling `sift` process briefly holding the lock.
    Write,
}

/// Open a DuckDB connection on `path` for the duration of `body`,
/// then drop it. The returned `Connection` is **never** exposed to
/// the caller past the closure boundary — the borrow checker forbids
/// it — which guarantees the OS file lock is released as soon as
/// `body` returns. This is the only approved entry point to a
/// `.duckdb` file inside the crate; never hold a `Connection` in a
/// struct field.
///
/// Lock contention from a sibling `sift` process is absorbed by
/// retrying `Connection::open_with_flags` with the
/// `[20, 80, 320] ms` backoff ladder (matches `bio-ncbi`'s
/// `RecordCache`). Three attempts, ≤ 420 ms total wait. After that
/// the underlying `duckdb::Error` bubbles up wrapped in
/// [`SiftError::Io`].
///
/// Use [`DuckAccess::Read`] for any SELECT-only path, [`DuckAccess::Write`]
/// for schema init / INSERT / UPDATE. A `body` that issues writes
/// under `Read` is rejected by DuckDB at execute time.
pub fn with_duckdb<T>(
    path: &Path,
    access: DuckAccess,
    body: impl FnOnce(&duckdb::Connection) -> Result<T, SiftError>,
) -> Result<T, SiftError> {
    let conn = open_duckdb_retrying(path, access)?;
    let out = body(&conn);
    // Drop is explicit for clarity; the connection (and its lock)
    // would be released here anyway when `conn` goes out of scope.
    drop(conn);
    out
}

fn open_duckdb_retrying(
    path: &Path,
    access: DuckAccess,
) -> Result<duckdb::Connection, SiftError> {
    const DELAYS_MS: [u64; 3] = [20, 80, 320];
    let mut last_err: Option<duckdb::Error> = None;
    for &delay_ms in &DELAYS_MS {
        // `Config::access_mode` consumes the config by value, so we
        // build a fresh one per attempt — `Config` is !Clone.
        let config = build_duckdb_config(access).map_err(|e| {
            SiftError::Io(format!("duckdb config ({access:?}): {e}"))
        })?;
        match duckdb::Connection::open_with_flags(path, config) {
            Ok(c) => return Ok(c),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
        }
    }
    Err(SiftError::Io(format!(
        "duckdb open {} ({:?}) after 3 retries: {}",
        path.display(),
        access,
        last_err.expect("at least one attempt failed when entering Err branch"),
    )))
}

fn build_duckdb_config(access: DuckAccess) -> Result<duckdb::Config, duckdb::Error> {
    let cfg = duckdb::Config::default();
    match access {
        DuckAccess::Read => cfg.access_mode(duckdb::AccessMode::ReadOnly),
        DuckAccess::Write => cfg.access_mode(duckdb::AccessMode::ReadWrite),
    }
}

/// Resolve `~/.sift/cache`. Production callers chain a per-source
/// subdir (e.g. `.join("cninfo")`); tests bypass this entirely by
/// using an isolated tempdir.
pub fn cache_root() -> Result<PathBuf, SiftError> {
    let home = dirs::home_dir()
        .ok_or_else(|| SiftError::Io("cannot resolve $HOME".into()))?;
    Ok(home.join(".sift").join("cache"))
}

/// `true` iff `path` exists and its mtime is within `ttl_secs` of now.
/// All error cases (missing file, unsupported platform, clock skew)
/// degrade silently to "not fresh" — the worst case is one extra fetch.
pub fn is_fresh(path: &Path, ttl_secs: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    mtime
        .elapsed()
        .map(|d| d.as_secs() < ttl_secs)
        .unwrap_or(false)
}

/// Write `bytes` to `path` atomically: create parents if needed, write
/// to `<path>.tmp`, then `rename` into place. On success no `.tmp`
/// file lingers; on failure the destination is untouched.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SiftError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SiftError::Io(format!("mkdir {}: {e}", parent.display())))?;
    }
    let tmp_ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("dat");
    let tmp = path.with_extension(format!("{tmp_ext}.tmp"));
    std::fs::write(&tmp, bytes)
        .map_err(|e| SiftError::Io(format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| SiftError::Io(format!("rename {} -> {}: {e}", tmp.display(), path.display())))?;
    Ok(())
}

/// Format a file's mtime as `YYYY-MM-DD HH:MM:SS` UTC. Best-effort —
/// any failure (no mtime, pre-epoch timestamp, unsupported platform)
/// collapses to a literal `"?"` so the caller can still emit a
/// meaningful message.
pub fn format_mtime_utc(path: &Path) -> String {
    let Ok(meta) = std::fs::metadata(path) else { return "?".into() };
    let Ok(mtime) = meta.modified() else { return "?".into() };
    let Ok(dur) = mtime.duration_since(UNIX_EPOCH) else { return "?".into() };
    format_utc_secs(dur.as_secs())
}

/// Format an epoch second into `YYYY-MM-DD HH:MM:SS` UTC. Pure function
/// exposed for direct unit testing; production callers go through
/// [`format_mtime_utc`].
pub(crate) fn format_utc_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let s_of_day = (secs % 86_400) as u32;
    let h = s_of_day / 3600;
    let m = (s_of_day % 3600) / 60;
    let s = s_of_day % 60;
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

/// Days since 1970-01-01 → proleptic Gregorian (year, month, day).
/// Reference: Howard Hinnant, "chrono-Compatible Low-Level Date Algorithms".
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    #[test]
    fn is_fresh_true_for_just_written_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("f");
        fs::write(&path, b"x").unwrap();
        assert!(is_fresh(&path, 3600));
    }

    #[test]
    fn is_fresh_false_for_expired_mtime() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("f");
        fs::write(&path, b"x").unwrap();
        let past = SystemTime::now() - Duration::from_secs(25 * 3600);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(past)).unwrap();
        assert!(!is_fresh(&path, 24 * 3600));
    }

    #[test]
    fn is_fresh_false_for_missing_file() {
        let tmp = TempDir::new().unwrap();
        assert!(!is_fresh(&tmp.path().join("nope"), 3600));
    }

    #[test]
    fn atomic_write_creates_dirs_and_no_tmp_lingers() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a").join("b").join("c.json");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        let tmp_sibling = path.with_file_name("c.json.tmp");
        assert!(!tmp_sibling.exists(), ".tmp must be renamed away");
    }

    #[test]
    fn atomic_write_overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("c.json");
        fs::write(&path, b"old").unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn atomic_write_clears_stale_tmp_via_rename() {
        // Simulate a previous interrupted writer that left a `.tmp` behind.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("c.json");
        let lingering = path.with_file_name("c.json.tmp");
        fs::write(&lingering, b"garbage").unwrap();
        atomic_write(&path, b"clean").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"clean");
        assert!(!lingering.exists(), "atomic_write replaces any pre-existing .tmp");
    }

    #[test]
    fn format_utc_secs_known_timestamps() {
        // Epoch.
        assert_eq!(format_utc_secs(0), "1970-01-01 00:00:00");
        // 2024-01-01 00:00:00 UTC.
        assert_eq!(format_utc_secs(1_704_067_200), "2024-01-01 00:00:00");
        // 2026-05-20 14:32:01 UTC.
        assert_eq!(format_utc_secs(1_779_287_521), "2026-05-20 14:32:01");
        // Leap day: 2024-02-29 00:00:00 UTC = 2024-01-01 + 59 days.
        assert_eq!(
            format_utc_secs(1_704_067_200 + 59 * 86_400),
            "2024-02-29 00:00:00"
        );
    }

    #[test]
    fn format_mtime_utc_round_trips_a_freshly_written_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("f");
        fs::write(&path, b"x").unwrap();
        let s = format_mtime_utc(&path);
        // Format check: `YYYY-MM-DD HH:MM:SS` = 19 chars; should not be `?`.
        assert_ne!(s, "?", "should resolve a real timestamp");
        assert_eq!(s.len(), 19, "actual: {s:?}");
    }

    #[test]
    fn format_mtime_utc_returns_question_mark_for_missing_file() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(format_mtime_utc(&tmp.path().join("nope")), "?");
    }

    // ------------------------------------------------------------------
    // with_duckdb
    // ------------------------------------------------------------------

    #[test]
    fn with_duckdb_write_creates_file_and_runs_schema() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("t.duckdb");
        with_duckdb(&path, DuckAccess::Write, |c| {
            c.execute_batch("CREATE TABLE t (k INT PRIMARY KEY, v TEXT);")
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            c.execute(
                "INSERT INTO t VALUES (1, 'hello')",
                duckdb::params![],
            )
            .map_err(|e| SiftError::Io(format!("{e}")))?;
            Ok(())
        })
        .unwrap();
        assert!(path.exists());
    }

    #[test]
    fn with_duckdb_read_observes_prior_write_after_drop() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("t.duckdb");
        with_duckdb(&path, DuckAccess::Write, |c| {
            c.execute_batch("CREATE TABLE t (k INT);")
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            c.execute("INSERT INTO t VALUES (42)", duckdb::params![])
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            Ok(())
        })
        .unwrap();
        // Write connection is dropped at this point — file lock free.
        let v = with_duckdb(&path, DuckAccess::Read, |c| {
            let mut s = c
                .prepare("SELECT k FROM t")
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            let mut rows = s
                .query([])
                .map_err(|e| SiftError::Io(format!("{e}")))?;
            let r = rows
                .next()
                .map_err(|e| SiftError::Io(format!("{e}")))?
                .expect("row");
            let k: i32 = r.get(0).map_err(|e| SiftError::Io(format!("{e}")))?;
            Ok(k)
        })
        .unwrap();
        assert_eq!(v, 42);
    }

    #[test]
    fn with_duckdb_read_rejects_write_attempts() {
        // DuckDB enforces ACCESS_MODE: an INSERT under READ_ONLY must
        // fail at execute time, which is the early-bug-detection
        // benefit of plumbing the mode through `with_duckdb`.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("t.duckdb");
        with_duckdb(&path, DuckAccess::Write, |c| {
            c.execute_batch("CREATE TABLE t (k INT);")
                .map_err(|e| SiftError::Io(format!("{e}")))
        })
        .unwrap();
        let err = with_duckdb(&path, DuckAccess::Read, |c| {
            c.execute("INSERT INTO t VALUES (1)", duckdb::params![])
                .map_err(|e| SiftError::Io(format!("{e}")))
        })
        .unwrap_err();
        let msg = format!("{err}");
        // Match either "read-only" or "read only" — DuckDB error
        // phrasing varies by version; both spellings appear upstream.
        let m = msg.to_lowercase();
        assert!(
            m.contains("read-only") || m.contains("read only"),
            "expected DuckDB read-only rejection, got: {msg}"
        );
    }

    #[test]
    fn with_duckdb_propagates_body_errors_and_drops_connection() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("t.duckdb");
        let err = with_duckdb(&path, DuckAccess::Write, |_c| {
            Err::<(), _>(SiftError::Io("body bailed".into()))
        })
        .unwrap_err();
        assert!(format!("{err}").contains("body bailed"));
        // After the failing call, the next open must succeed — proves
        // the connection (and its file lock) was released even on the
        // error path.
        with_duckdb(&path, DuckAccess::Write, |c| {
            c.execute_batch("CREATE TABLE t (k INT);")
                .map_err(|e| SiftError::Io(format!("{e}")))
        })
        .unwrap();
    }
}
