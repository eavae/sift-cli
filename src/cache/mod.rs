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

pub mod file;
pub mod record;
pub mod ttl;

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::error::SiftError;

// The DuckDB access primitive lives in `crate::db` (a neutral
// dependency shared by this cache module and the persistent
// `store`). Re-exported here so existing `cache::with_duckdb` /
// `super::with_duckdb` call sites — and `cache::record` — keep
// compiling unchanged.
pub use crate::db::{with_duckdb, DuckAccess};

/// 24 h.
pub const CACHE_TTL_SEARCH_SECS: u64 = 24 * 3600;

/// Resolve the user's home directory — the parent of the `.sift` tree.
///
/// `HOME` wins when set, then the OS default (`dirs::home_dir()` →
/// `%USERPROFILE%` on Windows, passwd/`$HOME` on unix). On unix this is
/// a no-op — `dirs` already reads `HOME` first — but on Windows `dirs`
/// resolves the profile folder via the shell API and *ignores* `HOME`,
/// so honoring it here is what lets integration tests (and power users)
/// redirect the whole `~/.sift` tree with one env var on every platform.
pub fn home_dir() -> Option<PathBuf> {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => Some(PathBuf::from(h)),
        _ => dirs::home_dir(),
    }
}

/// Resolve `~/.sift/cache`. Production callers chain a per-source
/// subdir (e.g. `.join("cninfo")`); tests bypass this entirely by
/// using an isolated tempdir.
pub fn cache_root() -> Result<PathBuf, SiftError> {
    let home =
        home_dir().ok_or_else(|| SiftError::Io("cannot resolve $HOME".into()))?;
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
}
