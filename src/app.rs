//! Process-wide ambient state assembled in `main.rs` and threaded
//! down by reference.
//!
//! Replaces three older global / Arc-based shapes that previously
//! coexisted:
//! - `sources::financial_source::Context` (HTTP + financials cache only)
//! - `cache::record::RecordCache::shared()` (`OnceLock` singleton)
//! - `sources::financial_source::REGISTRY` (`OnceLock` source list)
//!
//! Now: a single `AppContext` owns all of those as plain fields. Every
//! `fetch::*` and `commands::*` entry point takes `&AppContext`; nothing
//! reads a static global. Per-command construction lives in `main.rs`
//! (`run_search` / `run_report` / `run_announce`), each filling in only
//! the fields its command needs and leaving the rest at defaults.
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
//! `HttpClient` (so source tests that do live HTTP work), no caches,
//! no sources. Unit tests opt into whatever subset they need via
//! struct-update syntax.

use crate::cache::file::FileCache;
use crate::cache::record::RecordCache;
use crate::http::HttpClient;
use crate::sources::financial_source::FinancialSource;

/// Ambient state for the whole binary. Constructed once per command
/// in `main.rs`, borrowed everywhere downstream.
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
    /// Registered F2 sources, in dispatch order. Empty when the
    /// command isn't `report`; that's also what unit tests use when
    /// they want to test dispatch with an explicit slice.
    pub sources: Vec<Box<dyn FinancialSource>>,
}
