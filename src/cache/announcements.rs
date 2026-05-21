//! Cache-layer adapters for the F3 `announce` feature:
//!
//! - Permanent PDF binaries under `~/.sift/cache/announcements/`.
//!   `<announcementId>.pdf` is the canonical filename; existence
//!   (with `size > 0`) is the cache-hit predicate — no mtime, no
//!   checksum, because a cninfo `announcementId` references an
//!   immutable filing. See `docs/f3-announce/story-04-download-and-
//!   pdf-cache.md` §3.
//! - Row-metadata adapters [`put_meta_row`] / [`put_meta_rows`]
//!   that translate an [`AnnouncementRow`] into a
//!   `CacheKind::AnnounceMeta` entry in [`RecordCache`]. Command-
//!   layer code calls these so it never has to name `CacheKind` or
//!   construct cache bodies itself.
//!
//! All public PDF functions resolve paths via
//! [`crate::cache::cache_root`]; the `*_at` siblings take an explicit
//! root for unit tests and are `pub(crate)` rather than `pub`.

use std::path::{Path, PathBuf};

use crate::cache::record::{Kind as CacheKind, RecordCache};
use crate::domain::announcement::AnnouncementRow;
use crate::error::SiftError;

/// Subdirectory name under `~/.sift/cache/`. Pulled out as a constant
/// so the integration tests can grep for it from the outside.
const SUBDIR: &str = "announcements";

/// Resolve `~/.sift/cache/announcements/`. The directory may or may
/// not exist on disk — callers that write through here go through
/// [`crate::cache::atomic_write`], which `mkdir -p`s before writing.
pub fn cache_dir() -> Result<PathBuf, SiftError> {
    Ok(super::cache_root()?.join(SUBDIR))
}

/// Resolve `~/.sift/cache/announcements/<id>.pdf`. We force lowercase
/// `.pdf` (cninfo's `adjunctUrl` uses `.PDF`) for filesystem-tool
/// friendliness — `ls *.pdf` should match every file in this dir.
pub fn pdf_path(id: &str) -> Result<PathBuf, SiftError> {
    Ok(cache_dir()?.join(filename(id)))
}

/// `<id>.pdf` — canonical cache filename. Centralized so `download`,
/// `show`, and the eventual F4 reader agree byte-for-byte.
fn filename(id: &str) -> String {
    format!("{id}.pdf")
}

/// `true` iff the cached PDF for `id` exists with non-zero size. A
/// zero-byte file is treated as "not cached" so a previous
/// half-written / interrupted download (which `atomic_write` ought to
/// prevent, but defense in depth) does not poison subsequent calls.
pub fn is_cached(id: &str) -> bool {
    pdf_path(id).map(|p| is_cached_at(&p)).unwrap_or(false)
}

/// Path-level cache-hit predicate. Pulled out for unit tests so we
/// can exercise the zero-byte / missing / present cases without
/// touching `$HOME`.
pub(crate) fn is_cached_at(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// Copy the cached PDF for `id` into `dst_dir/<id>.pdf`. Creates
/// `dst_dir` (and any missing ancestors) if needed. Returns the
/// destination path so the caller can log it.
///
/// Plain `std::fs::copy` rather than hardlink/symlink: PDFs are
/// hundreds of KB at most and cross-filesystem hardlinks would fail
/// silently in surprising ways.
pub fn copy_to(id: &str, dst_dir: &Path) -> Result<PathBuf, SiftError> {
    let src = pdf_path(id)?;
    copy_to_at(&src, id, dst_dir)
}

/// Source-explicit sibling of [`copy_to`]. Production callers stay on
/// the wrapping `copy_to`; the unit test for "creates dst_dir" goes
/// through this seam so it does not depend on `$HOME`.
pub(crate) fn copy_to_at(src: &Path, id: &str, dst_dir: &Path) -> Result<PathBuf, SiftError> {
    std::fs::create_dir_all(dst_dir)
        .map_err(|e| SiftError::Io(format!("mkdir {}: {e}", dst_dir.display())))?;
    let dst = dst_dir.join(filename(id));
    std::fs::copy(src, &dst)
        .map_err(|e| SiftError::Io(format!("copy {} -> {}: {e}", src.display(), dst.display())))?;
    Ok(dst)
}

// ---------------------------------------------------------------------------
// Row-metadata adapters (RecordCache::AnnounceMeta)
// ---------------------------------------------------------------------------

/// Persist one [`AnnouncementRow`] into the shared [`RecordCache`]
/// under [`CacheKind::AnnounceMeta`]. Serialization failures are
/// silently dropped — the call is best-effort by contract (a future
/// network round-trip is always a valid fallback) and the cache layer
/// itself already emits `[warn]` on write failures.
pub fn put_meta_row(row: &AnnouncementRow) {
    let Ok(body) = serde_json::to_vec(row) else {
        return;
    };
    RecordCache::shared().put(CacheKind::AnnounceMeta, &[], &row.id, &body);
}

/// Bulk variant of [`put_meta_row`]. Routes through
/// [`RecordCache::put_many`] so one DuckDB connection serves the whole
/// batch — important on the announce paginate fallback path, which
/// invokes this once per ~30-row page.
pub fn put_meta_rows(rows: &[AnnouncementRow]) {
    if rows.is_empty() {
        return;
    }
    let items = rows.iter().filter_map(|row| {
        serde_json::to_vec(row)
            .ok()
            .map(|body| (row.id.clone(), body))
    });
    RecordCache::shared().put_many(CacheKind::AnnounceMeta, &[], items);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn filename_is_lowercase_pdf_extension() {
        assert_eq!(filename("1219506510"), "1219506510.pdf");
    }

    #[test]
    fn is_cached_at_returns_false_for_missing_file() {
        let tmp = TempDir::new().unwrap();
        assert!(!is_cached_at(&tmp.path().join("nope.pdf")));
    }

    #[test]
    fn is_cached_at_returns_false_for_zero_byte_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("empty.pdf");
        fs::write(&p, b"").unwrap();
        assert!(!is_cached_at(&p), "zero-byte file is not a valid cache hit");
    }

    #[test]
    fn is_cached_at_returns_true_for_non_empty_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("ok.pdf");
        fs::write(&p, b"%PDF-1.4\nhello").unwrap();
        assert!(is_cached_at(&p));
    }

    #[test]
    fn copy_to_at_copies_into_an_existing_dst_dir() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.pdf");
        fs::write(&src, b"payload").unwrap();
        let dst_dir = tmp.path().join("out");
        fs::create_dir_all(&dst_dir).unwrap();
        let dst = copy_to_at(&src, "abc", &dst_dir).unwrap();
        assert_eq!(dst, dst_dir.join("abc.pdf"));
        assert_eq!(fs::read(&dst).unwrap(), b"payload");
    }

    #[test]
    fn copy_to_at_creates_missing_dst_dir_recursively() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.pdf");
        fs::write(&src, b"payload").unwrap();
        // Two missing ancestors — `create_dir_all` should handle both.
        let dst_dir = tmp.path().join("nested").join("deeper");
        assert!(!dst_dir.exists());
        let dst = copy_to_at(&src, "x", &dst_dir).unwrap();
        assert!(dst_dir.is_dir(), "create_dir_all should have made it");
        assert_eq!(dst.file_name().unwrap(), "x.pdf");
    }

    #[test]
    fn copy_to_at_propagates_missing_source_as_io_error() {
        let tmp = TempDir::new().unwrap();
        let nonexistent = tmp.path().join("ghost.pdf");
        let err = copy_to_at(&nonexistent, "x", &tmp.path().join("out")).unwrap_err();
        match err {
            SiftError::Io(m) => assert!(m.contains("copy"), "msg: {m}"),
            other => panic!("expected Io error, got {other:?}"),
        }
    }
}
