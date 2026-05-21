//! Process-wide ambient state assembled in `main.rs` and threaded
//! down by reference.
//!
//! `AppContext` is the **cross-cutting infrastructure bag** — every
//! field is used by ≥2 commands. F2-specific state (the source
//! list) is **not** here; it lives in [`crate::fetch::report::ReportContext`],
//! which wraps `&AppContext` with the F2 sources slice. The split
//! keeps `AppContext` stable as new features land — F4/F5 add their
//! own per-feature contexts rather than growing this struct.
//!
//! ## Why no `Arc`
//!
//! Both cache types ([`FileCache`] / [`RecordCache`]) are thin — just a
//! `PathBuf`, with every operation opening a fresh connection or
//! resolving a fresh path. They are cheap to hold by value. The
//! `AppContext` itself is borrowed wherever it's needed, including
//! across `std::thread::scope` worker threads in the F2 dispatcher —
//! no clone is required.
//!
//! ## Defaults
//!
//! `AppContext::default()` produces an in-memory zero-state: a real
//! `HttpClient` (so source tests that do live HTTP work), no caches.
//! Unit tests opt into whatever subset they need via struct-update
//! syntax.

use crate::cache::file::FileCache;
use crate::cache::record::RecordCache;
use crate::http::HttpClient;

/// Cross-cutting state for the whole binary. Three fields, all used
/// by ≥2 commands: HTTP transport (3/3), file cache (3/3), record
/// cache (report + announce). Per-feature state hangs off feature
/// contexts (see [`crate::fetch::report::ReportContext`]).
#[derive(Default)]
pub struct AppContext {
    /// Shared HTTP client. Cheap to construct; we still keep one
    /// instance per command so connection pooling has scope to kick
    /// in across multiple symbols / periods within one invocation.
    pub http: HttpClient,
    /// Filesystem blob-by-name cache rooted at `~/.sift/cache/` (or a
    /// tempdir under test). Hosts F1 cninfo listings (`cninfo/…`) and
    /// F3 PDF binaries (`announcements/<id>.pdf`). `None` when
    /// `$HOME` was unresolvable at startup — fetch helpers degrade
    /// to no-cache mode in that case.
    pub file_cache: Option<FileCache>,
    /// DuckDB blob-by-key cache shared by F2 financials and F3
    /// announce metadata (`<cache_root>/records.duckdb`). `None` when
    /// the command didn't need a record cache or when opening the
    /// file failed (warn-and-continue path in `main.rs`).
    pub records_cache: Option<RecordCache>,
}
