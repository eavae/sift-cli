//! DuckDB access primitive — the one approved way to touch any
//! `.duckdb` file in the crate.
//!
//! Hoisted out of `cache::mod` so it is a **neutral** dependency: the
//! disposable record cache (`cache::record`) and the persistent,
//! user-curated fact store (`store`) both open DuckDB files, and
//! neither should depend on the other. Both depend downward on this
//! module instead.
//!
//! Two invariants this enforces:
//!
//! 1. **No long-lived `Connection` in a struct field.** [`with_duckdb`]
//!    opens a connection for the duration of the closure and drops it
//!    on return, releasing the OS file lock immediately — the only way
//!    parallel `sift` processes cooperate on one file.
//! 2. **Read/write intent is declared at acquire time.**
//!    [`DuckAccess::Read`] opens `READ_ONLY` (writes rejected by the DB
//!    at execute time); [`DuckAccess::Write`] takes the exclusive lock.

use std::path::Path;
use std::time::Duration;

use crate::error::SiftError;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
